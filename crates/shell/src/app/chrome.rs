//! 主题 / 外壳配色与标题栏：`ChromeColors` 语义取色、Mica 材质层、主题切换与
//! 偏好持久化、自绘标题栏（窗口控制钮 + 侧栏开关）与右上角 toast 通知层。

use super::*;

/// 窗口控制钮图标前景（Win11「注入感」）：只换色、粗细/几何不动。
/// - 关闭：空闲次要灰 → hover/按压 白 ×（红底白×，与 P1-7 `close_pressed` 搭配）
/// - 最小化/最大化：空闲次要灰 → hover/按压 主前景（灰底对比前景）
///
/// 按压态独立于 hover 跟随（`pressed || hovered`），保证压住不放时图标保持注入色。
/// 纯函数（不依赖 app 实体，供 `ui::icon_button` 的 `icon_color_for` `'static` 闭包复用；
/// `primary`/`minor` 由调用方在 render 期解析后传入）。
fn title_ctrl_fg(
    is_close: bool,
    hovered: bool,
    pressed: bool,
    primary: gpui::Hsla,
    minor: gpui::Hsla,
) -> gpui::Hsla {
    if hovered || pressed {
        if is_close {
            white()
        } else {
            primary
        }
    } else {
        minor
    }
}

impl EditorApp {
    /// 切换侧边栏「展开/收起」。
    /// `#[doc(hidden)] pub`：`tests/palette.rs` 侧栏几何断言（§8.6 外提清单）。
    ///
    /// 视频选项卡激活时（自动隐藏生效中）点击 = **显式打开**：把 `video_hide_override`
    /// 置 true 挂起自动隐藏、`open` 置 true，侧边栏出现；再点一次走普通翻转关闭（同时
    /// 复位 override）。非视频上下文行为与旧版完全一致（普通翻转 `open`）。
    #[doc(hidden)]
    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        // 先判状态再写字段（`active_media_is_video` 只读借用 self.model，先取 bool
        // 避免与后续 &mut self.sidebar 冲突）。
        let video_autohide_active = self.active_media_is_video() && !self.sidebar.video_hide_override;
        if video_autohide_active {
            // 视频自动隐藏生效中 → 本次点击=显式打开（不翻 open、挂起自动隐藏）。
            self.sidebar.open = true;
            self.sidebar.video_hide_override = true;
        } else {
            self.sidebar.open = !self.sidebar.open;
            self.sidebar.video_hide_override = false;
        }
        cx.notify();
    }

    /// 当前生效主题对应的外壳配色（`ui::theme::Theme::global(cx).chrome` 单一来源；
    /// 默认零覆盖时与旧 `ChromeColors::for_mode(model.dark_mode)` 逐字节一致，
    /// 自定义重点色经 `refresh_theme_ui` 的 `Theme::apply` 合并后由此处全程可见——
    /// 自定义 `chrome.*` 覆盖对外壳的消费面统一生效（P1-1）。
    pub(super) fn chrome(&self, cx: &App) -> ChromeColors {
        ui::theme::Theme::global(cx).chrome
    }

    /// 窗口整体背景（编辑区以外的画布）。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 主题切换背景断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn window_bg(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).window_bg.into()
    }

    /// Win11 Mica chrome 透明度（亮 0.86 / 暗 0.82）：半透明才能透出系统 Mica 材质。
    pub(super) fn mica_alpha(&self, cx: &App) -> f32 {
        if ui::theme::Theme::global(cx).is_dark() {
            ui::theme::MICA_CHROME_ALPHA_DARK
        } else {
            ui::theme::MICA_CHROME_ALPHA_LIGHT
        }
    }
    /// 标题栏/选项卡栏/侧栏工具条背景（Mica 半透明版；暗色主题改用 theme `chrome_bg`
    /// #191A1B（原 config `CHROME_BG_DARK`，常量已删）的不透明统一底色）。
    pub(super) fn mica_bar_bg(&self, cx: &App) -> gpui::Hsla {
        if ui::theme::Theme::global(cx).is_dark() {
            ui::theme::Theme::global(cx).chrome.chrome_bg.into()
        } else {
            self.bar_bg(cx).opacity(self.mica_alpha(cx))
        }
    }

    /// 自绘 Mica 材质层：窗口基底用 N 段半透明色带叠出柔和垂向渐变（纯应用内绘制，
    /// 不调用系统 Mica/acrylic/DWM）。放在最底层，chrome 半透明表面叠其上形成玻璃感。
    ///
    /// 每帧调用；段参数（64 段：顶边/高/颜色）只取决于「窗口高度桶 + 明暗主题」——
    /// 已算好的段数组缓存于 `material_layer_cache`（`(高度桶, dark_mode) → Rc<Vec<MaterialBand>>`），
    /// 缓存命中时每帧只 clone 一次 `Rc`、由 [`MaterialLayer`] 元素直接画 64 个 quad，
    /// **不再重建 64 个 band Div / 重算插值**；只有高度桶或明暗主题变化才重算段数组。
    /// 失效面枚举：窗口高度（桶）变化 / 明暗主题切换 两维 → 重算，其余（滚动、内容变化、
    /// 鼠标移动、桶内小幅 resize）一律复用。
    pub(super) fn render_material_layer(&mut self, window: &Window, cx: &App) -> impl IntoElement {
        let dark = ui::theme::Theme::global(cx).is_dark();
        let chrome = self.chrome(cx);
        let top: gpui::Hsla = chrome.material_top.into();
        let bottom: gpui::Hsla = chrome.material_bottom.into();
        let h = window.bounds().size.height.as_f32();
        // 高度桶：按 8px 量化——连续拖拽 resize 时不必每 1px 重算 64 段；桶内高度差
        //（≤±4px）由最后一段锚底部自适应，肉眼不可辨。
        const BUCKET_PX: f32 = 8.0;
        let h_bucket = (h / BUCKET_PX).round() * BUCKET_PX;
        // 取（或重算）段数组：命中缓存只 clone `Rc`，跨越桶/主题才进 build_material_bands。
        let bands = match &self.material_layer_cache {
            Some((kh, kd, cached)) if *kh == h_bucket && *kd == dark => cached.clone(),
            _ => {
                let bands = build_material_bands(h_bucket, top, bottom);
                self.material_layer_cache = Some((h_bucket, dark, bands.clone()));
                bands
            }
        };
        // 外层保持 absolute 铺底（与旧结构一致）；渐变段由 `MaterialLayer` 一次直画。
        div().absolute().top_0().left_0().right_0().bottom_0().child(MaterialLayer { bands })
    }

    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景。
    pub(super) fn bar_bg(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).bar_bg.into()
    }

    /// 工具条分隔边框。
    pub(super) fn bar_border(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).bar_border.into()
    }

    /// 区域分隔发丝线（`ChromeColors::hairline` 语义 token、按明暗主题取色，
    /// **不透明** —— 半透明叠不同底色会混出各自的灰色，这里统一成全屏一致的发丝线；
    /// 标题栏底 / 编辑区顶边 / 侧栏右缘 / 状态栏边框共用）。
    pub(super) fn hairline(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).hairline.into()
    }

    /// 编辑器/预览面板背景（暗色主题 = theme `editor_bg`（原 config `EDITOR_BG_DARK`
    /// #121314，常量已删）统一底色）。
    pub(super) fn panel_bg(&self, cx: &App) -> gpui::Hsla {
        ui::shell::content_bg(
            ui::theme::Theme::global(cx).is_dark(),
            self.chrome(cx).panel_bg.into(),
        )
    }

    /// 主要文字颜色。
    pub(super) fn text_primary(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).text_primary.into()
    }

    /// 次要/弱化文字颜色。
    pub(super) fn text_minor(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).text_minor.into()
    }

    /// 芯片上的文字颜色。
    pub(super) fn chip_text(&self, cx: &App) -> gpui::Hsla {
        self.chrome(cx).chip_text.into()
    }

    /// 切换黑白（深色/浅色）主题：作为 light↔dark 两态切换（退出「跟随」），
    /// 偏好记为当前生效的强制档。驱动自研主题（`ui::theme`）双轨同色；
    /// 编辑器语法配色随明暗切换为 GitHub Light / Dark（见 [`syntax`]）。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 主题切换不覆盖显式 PDF 背景断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_theme(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let dark_mode = !self.model.borrow().dark_mode;
        self.model.borrow_mut().dark_mode = dark_mode;
        // 手动切换 = 退出跟随：记为强制档并写盘。
        self.model.borrow_mut().theme_pref = if dark_mode {
            ThemePref::Dark
        } else {
            ThemePref::Light
        };
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 进入「跟随系统」模式：主题改由系统深浅色决定，并立即按当前系统落地一次。
    pub(super) fn follow_system_theme(&mut self, cx: &mut Context<Self>) {
        self.model.borrow_mut().theme_pref = ThemePref::Follow;
        self.model.borrow_mut().dark_mode = ThemePref::Follow.resolved_dark(cx.window_appearance());
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 把当前「有效主题」（明暗档 + 人格预设 + 自定义覆盖）落到底到全局主题 + 刷新
    /// 所有预览背景（toggle / 跟随系统 / 启动恢复 / 导入设置共用同一入口）。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 主题刷新后落位断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn refresh_theme_ui(&mut self, cx: &mut Context<Self>) {
        // 从模型组装主题规格：mode = 当前明暗档；preset = 按 model.theme_preset 解析
        // （未知 id 兜底 `PRESETS[0]` = default）；custom = model.theme_custom（经
        // `ColorToken` 校验只保留合法 key，把运行时 `String` 键回收成 `&'static str`
        // 键——不泄漏、未知 key 丢弃，与 `merge_spectrum`/`merge_chrome` 跳过语义一致）。
        // 合并只在本次调用跑一次，渲染只读 `Theme::global(cx)` 产物（性能纪律：非每帧）。
        let model = self.model.borrow();
        let mode = if model.dark_mode {
            ui::theme::ThemeMode::Dark
        } else {
            ui::theme::ThemeMode::Light
        };
        let preset = ui::theme::PRESETS
            .iter()
            .find(|p| p.id == model.theme_preset)
            .unwrap_or(&ui::theme::PRESETS[0]);
        let custom = model
            .theme_custom
            .iter()
            .filter_map(|(k, v)| ui::theme::ColorToken::from_key(k).map(|tok| (tok.key(), v.clone())))
            .collect();
        drop(model);
        ui::theme::Theme::apply(ui::theme::ThemeSpec { mode, preset, custom }, cx);
        // 刷新所有预览/WYSIWYG 的调色主题：`WyTheme` 在创建时从 `StyleParams` 一次性
        // 派生并缓存，主题切换后必须重建（MD / EPUB 统一走 `WyEditor`；`MarkdownPreview`
        // 后端已删，M5-B5 P4-2）。PDF 画布用灰色系（window_bg）与"纸面"卡片分隔。
        let canvas_bg = self.window_bg(cx);
        for tab in &self.model.borrow().tabs {
            // A2/A4：右栏统一 WyEditor 预览（md_wy）与 WYSIWYG 活区（live2）的调色主题
            //（原先只刷背景，内容色滞后 →"切换黑白后预览不完全重染色/用旧色"）。
            if let Some(wy) = tab.payload.md_wy.clone() {
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
            match &tab.payload.reader {
                MediaReader::Pdf(pdf) => {
                    let pv = pdf.clone();
                    // R6 守卫：PDF 画布背景**显式设置过**（非「默认」）→ 主题切换不得覆盖
                    // 画布色；仅「默认」档才跟随主题（回落 default_canvas = 新主题 window_bg）。
                    if self.model.borrow().pdf_bg == PdfBgPref::Default {
                        pv.update(cx, |view, cx| {
                            view.clear_background();
                            cx.notify();
                        });
                    }
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

    /// 「主题…」子菜单预设选择：写 `model.theme_preset` → 统一走 [`Self::refresh_theme_ui`]
    /// 重建有效主题 → 持久化。预设选择与「跟随系统/亮/暗」三态**正交**（只动
    /// `theme_preset`，不动 `theme_pref`/`dark_mode`）；自定义重点色叠加在当前预设上。
    pub(super) fn set_theme_preset(&mut self, id: &str, cx: &mut Context<Self>) {
        self.model.borrow_mut().theme_preset = id.to_string();
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 写自定义重点色（取色回调 / 测试共用）：校验 `#rrggbb` 合法才落
    /// `model.theme_custom["colors.accent"]` → apply → persist。**非法 hex / 空 → 不落盘、
    /// 保持原值**（评审 P1-7：保留最近有效值；非法值经 `merge_spectrum` 也会被跳过，此处
    /// 前置拒绝保证存储面也干净）。
    pub(super) fn set_theme_accent(&mut self, hex: String, cx: &mut Context<Self>) {
        if ui::theme::ColorToken::parse_hex(&hex).is_none() {
            return;
        }
        self.model
            .borrow_mut()
            .theme_custom
            .insert("colors.accent".to_string(), hex);
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 还原重点色：删 `model.theme_custom["colors.accent"]` → apply → persist（`merge_spectrum`
    /// 回落 preset 值）。无自定义重点色时 no-op（菜单项置灰、点击期兜底拒绝的表现）。
    pub(super) fn reset_theme_accent(&mut self, cx: &mut Context<Self>) {
        if self.model.borrow_mut().theme_custom.remove("colors.accent").is_none() {
            return;
        }
        self.refresh_theme_ui(cx);
        self.persist_prefs();
    }

    /// 「自定义重点色…」取色入口：镜像 [`Self::pick_custom_pdf_bg`] 的 **K167 姿势**——
    /// Windows 原生取色（CHOOSECOLORW，FFI 在 `shell_pdf::bg::pick_color`）移到**后台线程**，
    /// 结果经 `futures::oneshot` + `window.spawn(...).detach()` 回传主线程再落地
    /// ([`Self::set_theme_accent`])。原因同 PDF：事件帧持有 gpui App RefCell，模态消息泵
    /// 同线程重入窗程 → 异步续体二次 `update_entity` 会 `borrow_mut` panic。确定/取消由
    /// `pick_color` 返回 `None`（取消）区分；取消不落盘、保持原值。
    pub(super) fn pick_custom_theme_accent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(windows, not(test)))]
        {
            use raw_window_handle::HasWindowHandle as _;
            // 所有者句柄：当前窗口 HWND（`pick_custom_pdf_bg` 同款提取姿势）——让取色
            // 对话框模态挂在本窗口上；取不到 → 零所有者仍可弹（顶层独立对话框）。
            let mut owner: isize = 0;
            if let Ok(handle) = window.window_handle()
                && let raw_window_handle::RawWindowHandle::Win32(win32) = From::from(handle)
            {
                owner = win32.hwnd.get();
            }
            // 初始色 = 当前全局 accent（`Theme::apply` 已合并 preset+custom 的产物；
            // 无自定义值时即 preset 值）。
            let initial: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
            let (tx, rx) = futures::channel::oneshot::channel();
            let entity = cx.entity();
            let _ = std::thread::Builder::new()
                .name("theme-accent-dialog".into())
                .spawn(move || {
                    let r = shell_pdf::bg::pick_color(Some(initial), owner);
                    let _ = tx.send(r);
                });
            window
                .spawn(cx, async move |cx| {
                    if let Ok(Some(hex)) = rx.await {
                        let _ = cx.update_window_entity(&entity, move |this, _window, cx| {
                            this.set_theme_accent(hex, cx);
                        });
                    }
                })
                .detach();
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = (window, cx);
        }
    }

    /// 汇总当前偏好（主题三态 / 字号 / 侧栏宽度 / 会话与托盘开关 / 打开文件夹根 /
    /// 最近打开），供持久化与「导出设置」（`settings_xfer`）共用同一口径——导出即
    /// 「当前交互态」，两者始终一致。
    /// 逻辑在 [`AppModel::collect_prefs`]（shell_core::actions）；此处 glue。
    pub(super) fn collect_prefs(&self) -> UserPrefs {
        let host: &dyn AppHost = self;
        self.model.borrow().collect_prefs(host)
    }

    /// 把当前偏好（主题三态 / 字号 / 侧栏宽度 / 最近打开）原子写盘。
    /// 侧栏宽度只在用户手动拖过（`width_auto == false`）时才持久化，
    /// 否则下次启动仍保持随内容自适应。
    /// **测试构建不写盘**（config crate 是依赖编译、不受其自身 `cfg!(test)` 门控，
    /// 若这里写盘会污染真实 `%APPDATA%\vuick\config.json`，见 K8）。
    /// 逻辑在 [`AppModel::persist_prefs`]；此处 glue。
    pub(super) fn persist_prefs(&self) {
        let host: &dyn AppHost = self;
        self.model.borrow().persist_prefs(host);
    }

    /// 构建单个窗口控制钮（最小化/最大化/关闭共用）：
    /// - 点击/双击/键盘语义由平台原生处理（`window_control_area` → WM_NCHITTEST 返回
    ///   HTMINBUTTON / HTMAXBUTTON / HTCLOSE，系统完成动作，应用层不重复处理）；
    /// - hover 背景淡入（P2-10 统一 spring，关闭钮红、其余灰）；
    /// - 按压底色 keyed「press」（P1-7 `pressed_fill` / 关闭钮 `close_pressed`），按下/
    ///   松开/移出三事件翻转 + `cx.notify(view_id)`（等效旧 CSS `.active` 的「按下即换底、
    ///   松开还原」模型，由 `ui::icon_button` `.pressed(...)` 接线）。
    ///
    /// 图标色按元素级 hover/pressed 态分支显式传 `Icon::text_color`——parent 的
    /// `text_color` 不会级联覆盖子元素 Icon 的显式色，必须在分支里决定；测试探针
    /// `last_title_ctrl_fg` 由 `.icon_color_for` 闭包（`.build()` 期同步执行）落盘。
    pub(super) fn title_ctrl_button(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        kind: WinCtrlKind,
        icon: IconName,
    ) -> gpui::AnyElement {
        let c = self.chrome(cx);
        let is_close = kind.is_close();
        let primary = self.text_primary(cx);
        let minor = self.text_minor(cx);
        // 测试可读：本帧解析出的图标前景色在 `icon_color_for` 闭包写入 probe（闭包在
        // `.build(window, cx)` 同步调用 = render 期），随构建一并落盘 app 实体——
        // 断言「注入」仍走真实渲染路径。
        // 复用宿主持有的共享探针（`title_ctrl_fg_probe`）：最小化/最大化/关闭三钮每帧
        // 只 clone（Rc 原子计数、零分配），不再各自新建 `Rc<RefCell>`。渲染路径不变：
        // 闭包在 `.build()` 期同步写入本 kind 字段 → 构建后按当前 kind 合并进
        // `last_title_ctrl_fg`。
        let probe = self.title_ctrl_fg_probe.clone();
        let pb = probe.clone();
        let el = ui::icon_button(kind.id(), icon)
            .wh(40., 32.)
            .icon_size(14.)
            .icon_color_for(move |hovered, pressed| {
                let color = title_ctrl_fg(is_close, hovered, pressed, primary, minor);
                let mut snap = pb.borrow_mut();
                match kind {
                    WinCtrlKind::Minimize => snap.minimize = color,
                    WinCtrlKind::Maximize => snap.maximize = color,
                    WinCtrlKind::Close => snap.close = color,
                }
                color
            })
            .hover_fill(Some(kind.hover_bg(c)))
            .pressed(Some(kind.active_bg(c)))
            .native(kind.area())
            .build(window, cx);
        // 测试可读：按「当前 kind」把解析出的前景色合并写进 `last_title_ctrl_fg`——
        // 三钮每次只覆盖自己那一字段，保持其余字段（首帧三钮在各自 render 落盘）。
        let cur = *probe.borrow();
        let snap = self.last_title_ctrl_fg.get_or_insert_with(TitleCtrlIconFg::default);
        match kind {
            WinCtrlKind::Minimize => snap.minimize = cur.minimize,
            WinCtrlKind::Maximize => snap.maximize = cur.maximize,
            WinCtrlKind::Close => snap.close = cur.close,
        }
        el
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
        let text_minor = self.text_minor(cx);
        let c = self.chrome(cx);
        let hover_fill: gpui::Hsla = c.hover_fill.into();
        // 侧边栏「展开/收起」开关：收拢时显示打开图标（点击展开），展开时显示收起图标。
        // 自绘图标按钮（非 window_control_area，普通点击交互）。P2-10：hover 淡入；
        // id/调试选择器/圆角（`gpui::rems(0.375)`=gpui `rounded_md`）/hover 弹簧全由
        // `ui::icon_button` 接管。
        let sidebar_toggle = ui::icon_button(
            "title-sidebar-toggle",
            if self.sidebar.open {
                IconName::PanelLeftClose
            } else {
                IconName::PanelLeftOpen
            },
        )
        .wh(30., 24.)
        .radius(gpui::rems(0.375)) // = `rounded_md()`（0.375rem，默认字号下同圆角）
        .icon_size(14.)
        .color(text_minor)
        .hover_fill(Some(hover_fill))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                this.toggle_sidebar(cx);
            }),
        )
        .build(window, cx);

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
            .bg(self.mica_bar_bg(cx))
            .border_b_1()
            .border_color(self.hairline(cx))
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
    /// 合成层约束：浮层一律走 `deferred(...)` 独立绘制层（与菜单
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

/// 计算 64 段材质带（构建期一次性；仅「尺寸桶 / 明暗主题」变化时才被调用）：
/// 颜色直接对 lightness 插值（材质色 h/s≈0），几何与旧的 64-Div 版逐项一致——
/// 末段锚到底（`last=true`，paint 期随实际窗高拉伸），其余每段向下多叠 1px
/// （N 段取整会在带间留 0~1px 缝透出底色成细线，多叠 1px 且后画者在上一层消除缝隙）。
fn build_material_bands(
    h_bucket: f32,
    top: gpui::Hsla,
    bottom: gpui::Hsla,
) -> std::rc::Rc<Vec<MaterialBand>> {
    // 分带数取 64：每根带跳变 <0.5/255 灰度，肉眼不可辨；
    // 若段数太少（如 8），明暗阶梯会透过半透明侧栏/工具条显现为横向分带。
    const N: usize = 64;
    let band_h = h_bucket / N as f32;
    let mut bands = Vec::with_capacity(N);
    for i in 0..N {
        let t = i as f32 / (N - 1) as f32;
        let lum = top.l + (bottom.l - top.l) * t;
        let top_px = i as f32 * band_h;
        if i == N - 1 {
            bands.push(MaterialBand {
                top: top_px,
                height: 0.0,
                color: gpui::hsla(0.0, 0.0, lum, 1.0),
                last: true,
            });
        } else {
            bands.push(MaterialBand {
                top: top_px,
                height: band_h + 1.0,
                color: gpui::hsla(0.0, 0.0, lum, 1.0),
                last: false,
            });
        }
    }
    std::rc::Rc::new(bands)
}

/// Mica 材质层绘制元素：paint 期把缓存的段数组直接画成 64 个 quad（`window.paint_quad`），
/// 不构造 64 个 Div 子元素——段参数在构建期一次性算好（`Rc` 共享、见
/// [`EditorApp::render_material_layer`] 的 `material_layer_cache`），每帧零段级分配，
/// 只克隆一次 `Rc`。几何与旧 64-Div 版逐项一致。
struct MaterialLayer {
    bands: std::rc::Rc<Vec<MaterialBand>>,
}

impl gpui::IntoElement for MaterialLayer {
    type Element = MaterialLayer;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for MaterialLayer {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        base::util::source_location()
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        // 填满外层 absolute 铺底容器（父尺寸的 100%，见外层 div.absolute() 定位）。
        let style = gpui::Style {
            size: gpui::Size::new(
                gpui::Length::Definite(gpui::relative(1.0)),
                gpui::Length::Definite(gpui::relative(1.0)),
            ),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut gpui::Window,
        _: &mut gpui::App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut gpui::Window,
        _: &mut gpui::App,
    ) {
        // 顺序画 0..N：后画者在上一层（对齐旧的 Div 兄弟压叠 → 段边界取下方段颜色）。
        let x = bounds.origin.x.as_f32();
        let width = bounds.size.width.as_f32();
        let top_y = bounds.origin.y.as_f32();
        let bottom_y = top_y + bounds.size.height.as_f32();
        for band in self.bands.iter() {
            let y = top_y + band.top;
            let height = if band.last {
                // 末段锚到底：随实际窗高拉伸（桶内 resize 无需重算段数组）。
                bottom_y - y
            } else {
                band.height
            };
            window.paint_quad(gpui::quad(
                gpui::Bounds::new(gpui::point(px(x), px(y)), gpui::Size::new(px(width), px(height))),
                gpui::Corners::default(),
                band.color,
                gpui::Edges::<gpui::Pixels>::default(),
                gpui::transparent_black(),
                gpui::BorderStyle::default(),
            ));
        }
    }
}
