//! 「帮助 → 关于」面板：VS Code 风格居中 overlay 卡（应用名+版本 / Rust 工具链 /
//! 构建档·目标 / OS 完整版本号），带「复制」「关闭」。
//!
//! 结构与既有居中面板（`ui::palette::CommandPalette` / `session_store::RecentPanel`）同款：
//! 宿主 `EditorApp` 以 `Option<Entity<AboutPanel>>` 持有，打开时创建 + 聚焦 + 订阅
//! `DismissEvent`；面板自身 `Render` 经 `ui::float_card::overlay_centered` 全窗 occlude
//! 居中挂载（K15/K44：occlude 拦截外点击向下的命中；`on_mouse_down_out` 关闭不在此实现，
//! 见本模块头注释「不做点击外部关闭」）。
//!
//! 「复制」把 [`about_text`] 整段写入系统剪贴板并给「已复制」反馈；「关闭」/Esc 发
//! `DismissEvent` → 宿主清引用释放实体（对齐命令面板/recent 面板生命周期）。

use super::*;
use gpui::{AnyElement, ClickEvent, EventEmitter, Hsla, KeyDownEvent};
use ui::button::{Button, ButtonVariants as _};
use ui::float_card::{FloatCardBg, FloatCardPad, FloatCardRadius, FloatCardShadow};
use ui::theme::Theme;

/// 关于面板卡片宽度（逻辑像素；信息行宽度余量充分，非整窗）。
const ABOUT_PANEL_W: f32 = 480.;
/// 卡片挂载 priority（与命令面板 / recent 面板同档 `210`，避免与右键菜单浮层叠加错序）。
const ABOUT_PANEL_PRIORITY: usize = 210;
/// 卡片距窗口顶的像素（与命令面板 `96.` 一致）。
const ABOUT_PANEL_TOP: f32 = 96.;

/// 构造「关于」完整文本（对齐 VS Code 信息块；**「复制」复制的正是整段**，渲染区逐行
/// 展示同一串 → 显示 == 复制不漂移）。纯函数，可单测；值来自 build.rs 构建期注入的
/// `env!("VUICK_*")`（取不到已回退空串）。
pub(crate) fn about_text() -> String {
    // Rust 工具链：build.rs 已回退 "unknown"。
    let rustc = env!("VUICK_RUSTC");
    // 构建档：debug/release 随编译 cfg 判别。
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    format!(
        "Vuick\
         \n\
         Version: {}\n\
         Rust: {rustc}\n\
         构建: {profile} · {}\n\
         OS: {} · {}",
        env!("CARGO_PKG_VERSION"),
        env!("VUICK_TARGET"),
        tray::os_version(),
        std::env::consts::ARCH,
    )
}

/// 「关于」面板（居中 overlay 卡）。宿主以 `Option<Entity<Self>>` 持有；
/// 关闭（「关闭」按钮 / Esc）发 `DismissEvent` → 宿主清引用释放实体。
pub(crate) struct AboutPanel {
    /// 面板焦点句柄（打开时聚焦；Esc 直达）。
    focus_handle: FocusHandle,
    /// 打开前的焦点句柄（关闭时归还；源帮助菜单条未持有焦点时为空）。
    previous_focus: Option<FocusHandle>,
    /// 是否已点过「复制」（按钮文案切「已复制」反馈）。
    copied: bool,
}

impl AboutPanel {
    pub(crate) fn new(previous_focus: Option<FocusHandle>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            previous_focus,
            copied: false,
        }
    }

    /// 测试可读：是否已点「复制」（按钮反馈态）。测试面（同 Button 扩展面惯例，
    /// `#[allow(dead_code)]`：生产构建不告警，测试构建消费）。
    #[allow(dead_code)]
    pub(crate) fn copied(&self) -> bool {
        self.copied
    }

    /// Esc → 关闭（与命令面板同款键盘路径）。
    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if e.keystroke.key == "escape" {
            self.close(window, cx);
        }
    }

    /// 关闭面板：归还打开前焦点（尚未被动作带走才还）+ 发 `DismissEvent`（宿主清引用）。
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.previous_focus.take() {
            handle.focus(window, cx);
        }
        cx.emit(DismissEvent);
    }

    /// 「复制」：把 [`about_text`] 整段写入系统剪贴板，按钮切「已复制」反馈。
    fn copy(&mut self, cx: &mut Context<Self>) {
        self.copied = true;
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(about_text()));
        cx.notify();
    }
}

impl Focusable for AboutPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for AboutPanel {}

impl Render for AboutPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        // 显式绑定 Hsla（既有习惯：`colors.fg.into()` 内联到 text_color 会触发 Into 推断
        // 二义（E0283），先绑定再直传）。
        let fg: Hsla = colors.fg.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        let text = about_text();

        // 信息行：`about_text` 逐行渲染（显示 == 复制同一串）。首行应用名加粗（正文色）；
        // 其余 `label: value` 行按 label 弱色、value 正文色（白色）对齐布局——
        // 「Version: 0.1.0」等值与正文同色。
        let mut rows: Vec<AnyElement> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let row: AnyElement = if i == 0 {
                div()
                    .text_color(fg)
                    .text_size(px(15.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(line.to_string())
                    .into_any_element()
            } else if let Some((label, value)) = line.split_once(": ") {
                div()
                    .flex()
                    .gap_2()
                    .child(div().text_color(fg_muted).child(label.to_string()))
                    .child(div().flex_1().text_color(fg).child(value.to_string()))
                    .into_any_element()
            } else {
                div()
                    .text_color(fg)
                    .child(line.to_string())
                    .into_any_element()
            };
            rows.push(row);
        }

        let copied = self.copied;
        let panel = ui::float_card(
            &colors,
            is_dark,
            FloatCardBg::Acrylic,
            FloatCardShadow::Double,
            FloatCardRadius::Lg,
            FloatCardPad::None,
        )
        .id("about-panel")
        .debug_selector(|| "about-panel".into())
        .track_focus(&self.focus_handle)
        .on_key_down(cx.listener(Self::on_key_down))
        .flex()
        .flex_col()
        .w(px(ABOUT_PANEL_W))
        .p(px(20.))
        .gap_y(px(12.))
        // 标题行：「关于」。
        .child(
            div()
                .flex()
                .items_center()
                .text_color(fg)
                .text_size(px(13.))
                .child("关于"),
        )
        // 信息行（显示 == 复制同一串）。
        .child(div().flex().flex_col().gap_y(px(6.)).children(rows))
        // 按钮行：复制（点后切「已复制」反馈）+ 关闭。行容器/按钮样式逐字对齐
        // WYSIWYG 编辑图片卡（image.rs：flex_row + gap_x(px(8.)) + justify_end；
        // ghost=取消族 / primary=保存族，复制=ghost、关闭=primary）。
        .child(
            div()
                .flex()
                .flex_row()
                .gap_x(px(8.))
                .justify_end()
                .child(
                    Button::new("about-copy")
                        .debug_selector(|| "about-copy".into())
                        .label(if copied { "已复制" } else { "复制" })
                        .ghost()
                        .h(px(26.))
                        .px(px(12.))
                        .on_click(cx.listener(
                            |this: &mut AboutPanel, _: &ClickEvent, _window, cx| this.copy(cx),
                        )),
                )
                .child(
                    Button::new("about-close")
                        .debug_selector(|| "about-close".into())
                        .label("关闭")
                        .primary()
                        .h(px(26.))
                        .px(px(12.))
                        .on_click(cx.listener(
                            |this: &mut AboutPanel, _: &ClickEvent, window, cx| {
                                this.close(window, cx)
                            },
                        )),
                ),
        );

        let win_w = window.bounds().size.width.as_f32();
        ui::float_card::overlay_centered(
            panel.into_any_element(),
            win_w,
            ABOUT_PANEL_W,
            ABOUT_PANEL_TOP,
            ABOUT_PANEL_PRIORITY,
        )
    }
}

impl EditorApp {
    /// 打开居中「关于」面板（帮助菜单「关于」项调用）：创建 `AboutPanel` 实体 + 聚焦 +
    /// 订阅 `DismissEvent`（关闭 → 清引用释放实体）。已打开 → 复用既有单槽（不叠多层）。
    /// 开面板**不再写状态栏文案**（旧 `model.status = "Vuick …"` 副作用由此取代）。
    pub(crate) fn open_about_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.about_panel.is_some() {
            return;
        }
        // 打开前的焦点 = 仍展开的帮助菜单句柄；面板关闭（动作未带走焦点）时归还它。
        let previous_focus = window.focused(cx);
        let panel = cx.new(|cx| AboutPanel::new(previous_focus, cx));
        let sub = cx.subscribe(&panel, |this, _e, _: &DismissEvent, cx| {
            this.about_panel = None;
            this._about_sub = None;
            cx.notify();
        });
        // 打开即聚焦面板（Esc/回车直达关闭路径）。
        let handle = panel.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.about_panel = Some(panel);
        self._about_sub = Some(sub);
        cx.notify();
    }

    /// 关闭「关于」面板（测试/复用入口）：让面板走 Esc 同款路径归还焦点并清引用。
    /// 生产构建当前无调用点（关闭由面板「关闭」按钮/Esc 直接驱动）→ `#[allow(dead_code)]`
    /// 挂测试面（Button 扩展面同款惯例）。
    #[allow(dead_code)]
    pub(crate) fn close_about_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.about_panel.clone() {
            p.update(cx, |p, cx| p.close(window, cx));
        }
    }
}
