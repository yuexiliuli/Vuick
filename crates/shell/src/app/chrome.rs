//! 主题 / 外壳配色与标题栏：`ChromeColors` 语义取色、Mica 材质层、主题切换与
//! 偏好持久化、自绘标题栏（窗口控制钮 + 侧栏开关）与右上角 toast 通知层。

use super::*;

impl EditorApp {
    /// 切换侧边栏「展开/收起」。
    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar.open = !self.sidebar.open;
        cx.notify();
    }

    /// 当前明暗模式对应的外壳配色（[`ui::theme::ChromeColors`] 单一来源，
    /// 值逐字节保持收编前硬编码；本系列后续步骤的配色地基）。
    pub(super) fn chrome(&self) -> ChromeColors {
        if self.dark_mode {
            ChromeColors::dark()
        } else {
            ChromeColors::light()
        }
    }

    /// 窗口整体背景（编辑区以外的画布）。
    pub(super) fn window_bg(&self) -> gpui::Hsla {
        self.chrome().window_bg.into()
    }

    /// Win11 Mica chrome 透明度（亮 0.86 / 暗 0.82）：半透明才能透出系统 Mica 材质。
    pub(super) fn mica_alpha(&self) -> f32 {
        if self.dark_mode {
            ui::theme::MICA_CHROME_ALPHA_DARK
        } else {
            ui::theme::MICA_CHROME_ALPHA_LIGHT
        }
    }
    /// 标题栏/选项卡栏/侧栏工具条背景（Mica 半透明版；暗色主题改用 theme `chrome_bg`
    /// #191A1B（原 config `CHROME_BG_DARK`，常量已删）的不透明统一底色）。
    pub(super) fn mica_bar_bg(&self) -> gpui::Hsla {
        if self.dark_mode {
            ui::theme::ChromeColors::for_mode(true).chrome_bg.into()
        } else {
            self.bar_bg().opacity(self.mica_alpha())
        }
    }

    /// 自绘 Mica 材质层：窗口基底用 N 段半透明色带叠出柔和垂向渐变（纯应用内绘制，
    /// 不调用系统 Mica/acrylic/DWM）。放在最底层，chrome 半透明表面叠其上形成玻璃感。
    pub(super) fn render_material_layer(&self, window: &Window) -> impl IntoElement {
        let chrome = self.chrome();
        let top: gpui::Hsla = chrome.material_top.into();
        let bottom: gpui::Hsla = chrome.material_bottom.into();
        let h = window.bounds().size.height.as_f32();
        // 分带数取 64：每根带跳变 <0.5/255 灰度，肉眼不可辨；
        // 若段数太少（如 8），明暗阶梯会透过半透明侧栏/工具条显现为横向分带。
        const N: usize = 64;
        let band_h = h / N as f32;
        let mut layer = div().absolute().top_0().left_0().right_0().bottom_0();
        for i in 0..N {
            let t = i as f32 / (N - 1) as f32;
            // 材质色是近中性灰（h/s≈0），直接对 lightness 插值即可
            let lum = top.l + (bottom.l - top.l) * t;
            let band = gpui::hsla(0.0, 0.0, lum, 1.0);
            let mut b = div().absolute().left_0().right_0().bg(band);
            if i == N - 1 {
                // 最后一根锚到底，保证渐变终点精确等于 bottom 色
                b = b.top(px(i as f32 * band_h)).bottom_0();
            } else {
                // 每根向下多叠 1px：N 段取整会在带间留 0~1px 缝透出底色成细线，
                // 多叠 1px 且后画者在上一层，彻底消除缝隙。
                b = b.top(px(i as f32 * band_h)).h(px(band_h + 1.0));
            }
            layer = layer.child(b);
        }
        layer
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    pub(super) fn bar_bg(&self) -> gpui::Hsla {
        self.chrome().bar_bg.into()
    }

    /// 工具条分隔边框。
    pub(super) fn bar_border(&self) -> gpui::Hsla {
        self.chrome().bar_border.into()
    }

    /// 区域分隔发丝线（`ChromeColors::hairline` 语义 token、按明暗主题取色，
    /// **不透明** —— 半透明叠不同底色会混出各自的灰色，这里统一成全屏一致的发丝线；
    /// 标题栏底 / 编辑区顶边 / 侧栏右缘 / 状态栏边框共用）。
    pub(super) fn hairline(&self) -> gpui::Hsla {
        self.chrome().hairline.into()
    }

    /// 编辑器/预览面板背景（暗色主题 = theme `editor_bg`（原 config `EDITOR_BG_DARK`
    /// #121314，常量已删）统一底色）。
    pub(super) fn panel_bg(&self) -> gpui::Hsla {
        ui::shell::content_bg(self.dark_mode, self.chrome().panel_bg.into())
    }

    /// 主要文字颜色。
    pub(super) fn text_primary(&self) -> gpui::Hsla {
        self.chrome().text_primary.into()
    }

    /// 次要/弱化文字颜色。
    pub(super) fn text_minor(&self) -> gpui::Hsla {
        self.chrome().text_minor.into()
    }

    /// 芯片上的文字颜色。
    pub(super) fn chip_text(&self) -> gpui::Hsla {
        self.chrome().chip_text.into()
    }

    /// 激活选项卡背景。
    pub(super) fn tab_active_bg(&self) -> gpui::Hsla {
        self.chrome().tab_active_bg.into()
    }

    /// 激活选项卡边框。
    pub(super) fn tab_active_border(&self) -> gpui::Hsla {
        self.chrome().tab_active_border.into()
    }

    /// 切换黑白（深色/浅色）主题：作为 light↔dark 两态切换（退出「跟随」），
    /// 偏好记为当前生效的强制档。驱动自研主题（`ui::theme`）双轨同色；
    /// 编辑器语法配色随明暗切换为 GitHub Light / Dark（见 [`syntax`]）。
    pub(super) fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        // 手动切换 = 退出跟随：记为强制档并写盘。
        self.theme_pref = if self.dark_mode {
            ThemePref::Dark
        } else {
            ThemePref::Light
        };
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 进入「跟随系统」模式：主题改由系统深浅色决定，并立即按当前系统落地一次。
    pub(super) fn follow_system_theme(&mut self, cx: &mut Context<Self>) {
        self.theme_pref = ThemePref::Follow;
        self.dark_mode = ThemePref::Follow.resolved_dark(cx.window_appearance());
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 把当前 `dark_mode` 落地到全局主题 + 刷新所有预览背景
    /// （toggle / 跟随系统 / 启动恢复共用同一入口）。
    pub(super) fn refresh_theme_ui(&mut self, cx: &mut Context<Self>) {
        ui::theme::Theme::set(
            if self.dark_mode {
                ui::theme::ThemeMode::Dark
            } else {
                ui::theme::ThemeMode::Light
            },
            cx,
        );
        // 刷新所有预览/WYSIWYG 的调色主题：`WyTheme` 在创建时从 `StyleParams` 一次性
        // 派生并缓存，主题切换后必须重建（MD / EPUB 统一走 `WyEditor`；`MarkdownPreview`
        // 后端已删，M5-B5 P4-2）。PDF 画布用灰色系（window_bg）与"纸面"卡片分隔。
        let canvas_bg = self.window_bg();
        for tab in &self.tabs {
            // A2/A4：右栏统一 WyEditor 预览（md_wy）与 WYSIWYG 活区（live2）的调色主题
            //（原先只刷背景，内容色滞后 →"切换黑白后预览不完全重染色/用旧色"）。
            if let Some(wy) = tab.md_wy.clone() {
                wy.update(cx, |ed, cx| {
                    ed.rebuild_theme_from_global(cx);
                    cx.notify();
                });
            }
            if let Some(ed) = tab.live2.clone() {
                ed.update(cx, |ed, cx| {
                    ed.rebuild_theme_from_global(cx);
                    cx.notify();
                });
            }
            // PDF / 图片 / 视频阅读器同样刷新背景（主题色变化后与画布保持一致；否则打开时
            // 记下的 `canvas_bg` 停留在旧主题，切主题后画面底仍旧色——视频曾整窗白底、图片
            // 白板不随暗色）。
            match &tab.reader {
                MediaReader::Pdf(pdf) => {
                    let pv = pdf.clone();
                    pv.update(cx, |view, cx| {
                        view.set_background(canvas_bg);
                        cx.notify();
                    });
                }
                MediaReader::Image(img) => {
                    let iv = img.clone();
                    iv.update(cx, |view, cx| {
                        view.set_background(canvas_bg);
                        cx.notify();
                    });
                }
                #[cfg(feature = "video")]
                MediaReader::Video(vid) => {
                    let vv = vid.clone();
                    vv.update(cx, |view, cx| {
                        view.set_background(canvas_bg);
                        cx.notify();
                    });
                }
                MediaReader::None => {}
            }
        }
        cx.notify();
    }

    /// 把当前偏好（主题三态 / 字号 / 侧栏宽度 / 最近打开）原子写盘。
    /// 侧栏宽度只在用户手动拖过（`width_auto == false`）时才持久化，
    /// 否则下次启动仍保持随内容自适应。
    /// **测试构建不写盘**（config crate 是依赖编译、不受其自身 `cfg!(test)` 门控，
    /// 若这里写盘会污染真实 `%APPDATA%\gpui-editor\config.json`，见 K8）。
    pub(super) fn persist_prefs(&self) {
        // 用 `cfg!(test)` 而不是 `#[cfg(test)] return;`：后者在测试构建里把后续写成
        // 不可达代码抛 `unreachable_code` 警告；cfg! 为运行时常量，不剪枝可达性。
        // 测试构建不写盘（config crate 是依赖编译、不受其自身 cfg!(test) 门控，
        // 若这里写盘会污染真实 `%APPDATA%\gpui-editor\config.json`，见 K8）。
        if cfg!(test) {
            return;
        }
        // 音乐播放器偏好（music_dir / music_liked）不由 shell 维护：本处写盘是 主题/字号/
        // 最近打开 等其它设置的持久化，须沿用已存值，否则每次保存都会把音乐目录/收藏
        // 覆盖回空（K10 派生状态一次性读回）。
        let existing = UserPrefs::load();
        UserPrefs {
            theme: self.theme_pref,
            font_size: Some(self.font_size),
            sidebar_width: if self.sidebar.width_auto {
                None
            } else {
                Some(self.sidebar.width)
            },
            // 会话恢复开关随其它偏好一并落盘（否则一次字号/主题保存就会把它重置回默认开）。
            session_restore: Some(self.session_restore),
            // 关闭到托盘开关随其它偏好一并落盘（否则一次字号/主题保存就把它重置回默认关）。
            close_to_tray: Some(self.close_to_tray),
            // 打开文件夹的根路径随偏好落盘（首次写前为空；路径失效时 `files.root` 为
            // None → 这里写 null，下次启动不再恢复该失效文件夹）。
            opened_folder: self
                .sidebar
                .files
                .root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            // 最近打开（文件夹 / 文件）：随偏好落盘（空 → `[]`），跨重启恢复（K11：
            // 只随打开/移除/清空这类状态变化写，不逐帧 / 不重复全量重写）。
            recent_folders: self.recent_folders.clone(),
            recent_files: self.recent_files.clone(),
            music_dir: existing.music_dir,
            music_liked: existing.music_liked,
            music_volume: existing.music_volume,
            music_last_track: existing.music_last_track,
            music_progress_secs: existing.music_progress_secs,
        }
        .save();
    }
    /// 窗口控制钮图标前景（Win11「注入感」）：只换色、粗细/几何不动。
    /// - 关闭：空闲次要灰 → hover/按压 白 ×（红底白×，与 P1-7 `close_pressed` 搭配）
    /// - 最小化/最大化：空闲次要灰 → hover/按压 主前景（灰底对比前景）
    ///
    /// 按压态独立于 hover 跟随（`pressed || hovered`），保证压住不放时图标保持注入色。
    pub(super) fn title_ctrl_fg(&self, is_close: bool, hovered: bool, pressed: bool) -> gpui::Hsla {
        if hovered || pressed {
            if is_close {
                white()
            } else {
                self.text_primary()
            }
        } else {
            self.text_minor()
        }
    }

    /// 构建单个窗口控制钮（最小化/最大化/关闭共用）：
    /// - 点击/双击/键盘语义由平台原生处理（`window_control_area` → WM_NCHITTEST 返回
    ///   HTMINBUTTON / HTMAXBUTTON / HTCLOSE，系统完成动作，应用层不重复处理）；
    /// - hover 背景淡入（P2-10 统一 spring，关闭钮红、其余灰）；
    /// - 按压底色 `.active`（P1-7 `pressed_fill` / 关闭钮 `close_pressed`）；按压态另存
    ///   keyed「press」供图标注入色跟随（测试平台等派发到元素的场景生效）。
    ///
    /// 图标色按元素级 hover/pressed 态分支显式传 `Icon::text_color`——parent 的
    /// `text_color` 不会级联覆盖子元素 Icon 的显式色，必须在分支里决定；
    /// `cx.notify(view_id)` 触发状态变化后的重绘。
    pub(super) fn title_ctrl_button(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        kind: WinCtrlKind,
        icon: IconName,
    ) -> gpui::AnyElement {
        let c = self.chrome();
        let hover_bg = kind.hover_bg(c);
        let view_id = window.current_view();
        let hover_key = format!("title-{}-hover", kind.id());
        let press_key = format!("title-{}-press", kind.id());
        let hovered_s = window.use_keyed_state(hover_key, cx, |_, _| false);
        let pressed_s = window.use_keyed_state(press_key, cx, |_, _| false);
        let hovered = *hovered_s.read(cx);
        let pressed = *pressed_s.read(cx);
        let icon_color = self.title_ctrl_fg(kind.is_close(), hovered, pressed);
        // 测试可读：本帧解析出的图标前景色落盘到 app 实体（断言「注入」走真实渲染路径）。
        let snap = self.last_title_ctrl_fg.get_or_insert_with(TitleCtrlIconFg::default);
        match kind {
            WinCtrlKind::Minimize => snap.minimize = icon_color,
            WinCtrlKind::Maximize => snap.maximize = icon_color,
            WinCtrlKind::Close => snap.close = icon_color,
        }
        div()
            .id(kind.id())
            .debug_selector(move || kind.id().to_string())
            .w(px(40.))
            .h(px(32.))
            .flex()
            .items_center()
            .justify_center()
            .cursor(CursorStyle::PointingHand)
            .active(|style| style.bg(kind.active_bg(c)))
            .child(Icon::new(icon).size(px(14.)).text_color(icon_color))
            .on_hover({
                let st = hovered_s.clone();
                move |hovered, _window, cx| {
                    st.update(cx, |h, _| *h = *hovered);
                    cx.notify(view_id);
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let st = pressed_s.clone();
                move |_ev, _window, cx| {
                    st.update(cx, |p, _| *p = true);
                    cx.notify(view_id);
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let st = pressed_s.clone();
                move |_ev, _window, cx| {
                    st.update(cx, |p, _| *p = false);
                    cx.notify(view_id);
                }
            })
            .on_mouse_up_out(MouseButton::Left, {
                let st = pressed_s.clone();
                move |_ev, _window, cx| {
                    st.update(cx, |p, _| *p = false);
                    cx.notify(view_id);
                }
            })
            .window_control_area(kind.area())
            // P2-10：hover 背景淡入。
            .with_spring(
                format!("title-{}-bg", kind.id()),
                hover_spring(if hovered { 1.0 } else { 0.0 }),
                move |this, v| this.bg(hover_bg.opacity(v)),
            )
            .into_any_element()
    }

    /// 自绘标题栏：无边框窗口下代替原生标题栏，颜色跟随黑白主题。
    /// 左侧为 文件/编辑/查看/帮助 菜单，中间空位为窗口拖拽区，
    /// 右侧为最小化 / 最大化 / 关闭按钮。
    ///
    /// 标题栏底边绘一条通宽发丝线（`bar_border`），分隔下方侧栏/选项卡栏内容区。
    /// 拖拽与窗口控制全部交给平台原生处理：各元素标上
    /// [`gpui::WindowControlArea`]，GPUI 在 WM_NCHITTEST 里返回对应
    /// HTCAPTION / HTMINMAXBUTTON，由系统完成移动（贴边吸附）、
    /// 最小化、最大化（含双击标题栏切换）与关闭。
    pub(crate) fn render_title_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_minor = self.text_minor();
        let c = self.chrome();
        let hover_fill: gpui::Hsla = c.hover_fill.into();
        // 窗口控制钮跨帧 hover 状态对应的视图 id（元素级状态变更后通知重绘）。
        let view_id = window.current_view();
        let icon_style = |icon: IconName| {
            Icon::new(icon).size(px(14.)).text_color(text_minor)
        };

        // 侧边栏「展开/收起」开关：收拢时显示打开图标（点击展开），展开时显示收起图标。
        // 用自绘图标按钮（非 window_control_area，普通点击交互）。P2-10：hover 淡入。
        let sidebar_toggle_hover =
            window.use_keyed_state("title-sidebar-toggle-hover", cx, |_, _| false);
        let sidebar_toggle_hovered = *sidebar_toggle_hover.read(cx);
        let sidebar_toggle = div()
            .id("title-sidebar-toggle")
            .debug_selector(|| "title-sidebar-toggle".into())
            .w(px(30.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_color(text_minor)
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = sidebar_toggle_hover.clone();
                move |hovered, _window, cx| {
                    st.update(cx, |h, _| *h = *hovered);
                    cx.notify(view_id);
                }
            })
            .child(icon_style(if self.sidebar.open {
                IconName::PanelLeftClose
            } else {
                IconName::PanelLeftOpen
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    this.toggle_sidebar(cx);
                }),
            )
            .with_spring(
                "title-sidebar-toggle-bg",
                hover_spring(if sidebar_toggle_hovered { 1.0 } else { 0.0 }),
                move |this, v| this.bg(hover_fill.opacity(v)),
            );

        let maximize_icon = if window.is_maximized() {
            IconName::WindowRestore
        } else {
            IconName::WindowMaximize
        };

        // 窗口控制钮（最小化/最大化/关闭）：工厂 `title_ctrl_button` 统一建，
        // hover/按压时图标「注入」对比前景（Win11 实心按钮观感），只换色、几何不动：
        // - 最小化/最大化：空闲次要灰 → hover/按压 主前景（灰底对比前景）
        // - 关闭：空闲次要灰 → hover/按压 白 ×（红底白×，与 P1-7 close_pressed 搭配）
        // 图标色按元素级 hover/pressed keyed 态分支显式传 `Icon::text_color`——
        // parent 的 text_color 不会级联覆盖子元素 Icon 的显式色，必须在分支里决定。
        let mut window_ctrl_children = vec![sidebar_toggle.into_any_element()];
        window_ctrl_children.push(
            self.title_ctrl_button(window, cx, WinCtrlKind::Minimize, IconName::WindowMinimize),
        );
        window_ctrl_children.push(
            self.title_ctrl_button(window, cx, WinCtrlKind::Maximize, maximize_icon),
        );
        window_ctrl_children.push(
            self.title_ctrl_button(window, cx, WinCtrlKind::Close, IconName::WindowClose),
        );

        div()
            .id("title-bar")
            .debug_selector(|| "title-bar".into())
            .h(px(32.))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            // 只留左内边距：Win11 Fluent 自绘标题栏下，窗口控制钮（最小化/最大化/
            // 关闭）右缘应贴齐窗口右缘，右内边距会在关闭钮右侧留 8px 可见 gap。
            .pl_2()
            .bg(self.mica_bar_bg())
            .border_b_1()
            .border_color(self.hairline())
            // 窗口最顶部最左：应用图标（内嵌 128×128 ICO 变色龙，ui::assets 同源）。
            .child(ui::assets::app_icon_element(px(18.)))
            .child(self.render_menu_buttons(cx))
            .child(
                div()
                    .id("title-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .children(window_ctrl_children)
    }

    /// 右上角 toast 层：卡片可见即渲染，点击卡片按「类型 + key」精确关闭。
    ///
    /// 合成层纪律：浮层一律走 `deferred(...)` 独立绘制层（与菜单
    /// `deferred(anchored())` 同款机制，见 src/ui/menu.rs），不要用裸 absolute。
    /// 每张卡片按 left = 窗口宽 - 卡片宽 - 右缘、right = 右缘、top = 4 + i*92、h 固定
    /// 显式定位（headless 与真机布局一致）；保留 .id + on_click 交互。
    ///
    /// 位置：右上角、但**下移避开 chrome**（标题栏 32 + 选项卡栏 32 = 64）——
    /// 卡片从 y=68 起叠在正文空内容区顶部，不遮挡右上角最小化/最大化/关闭按钮
    /// 与选项卡栏。gpu 合成路径实测可正常绘制（此区无平台层障碍）。
    pub(super) fn render_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.notifications.is_empty() {
            return div().into_any_element();
        }
        let width = window.bounds().size.width.as_f32();
        const CARD_W: f32 = 320.;
        const RIGHT: f32 = 4.;
        // 标题栏(32) + 选项卡栏(32) + 4 边距 → 卡片落在正文空内容区顶部，避免遮挡按钮。
        const TOP: f32 = 68.;
        // 卡片高度由内容自适应（消息可多行换行），STEP 仅作多卡竖排间距下限。
        const STEP: f32 = 104.;
        let mut layer = div().absolute().top_0().left_0().right_0().bottom_0();
        for (i, n) in self.notifications.iter().enumerate() {
            let id_type = n.id_type();
            let key = n.key().clone();
            let left = width - CARD_W - RIGHT;
            layer = layer.child(
                n.render_card(window, cx)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.notifications
                            .retain(|it| it.id_type() == id_type && it.key() == &key);
                        cx.notify();
                    }))
                    .absolute()
                    .top(px(TOP + i as f32 * STEP))
                    .left(px(left))
                    .right(px(RIGHT)),
            );
        }
        // 不用 .with_priority(100)（菜单已用 100）：toast 恒在最上
        gpui::deferred(layer).with_priority(200).into_any_element()
    }
}
