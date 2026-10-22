//! 文件树「新建/重命名」输入弹层：居中 Acrylic 卡片 + 单行输入框（复用 `ui::input`），
//! 回车/确定提交 → 回调宿主执行文件系统操作；失败在卡片内联提示（不静默丢反馈）；
//! Esc/取消/点外 → 关闭（发 `DismissEvent`，宿主清引用释放实体）。焦点管理同
//! [`recent_panel::RecentPanel`]（开前记焦点、动作未带走才归还）。

use super::*;
use gpui::{EventEmitter, KeyDownEvent, WeakEntity};
use ui::input::Input;
use ui::motion::hover_spring;

/// 弹层操作类型（决定标题 / 预填名 / 提交后动作）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileTreePromptKind {
    /// 在 `parent` 目录下新建文件（提交成功 → 自动打开成选项卡）。
    NewFile { parent: PathBuf },
    /// 在 `parent` 目录下新建文件夹。
    NewFolder { parent: PathBuf },
    /// 重命名 `path`（文件或目录）。
    Rename { path: PathBuf, is_dir: bool },
}

impl FileTreePromptKind {
    /// 卡片标题。
    fn title(&self) -> &'static str {
        match self {
            FileTreePromptKind::NewFile { .. } => "新建文件",
            FileTreePromptKind::NewFolder { .. } => "新建文件夹",
            FileTreePromptKind::Rename { .. } => "重命名",
        }
    }

    /// 预填默认名：新建文件「新建文件.md」/ 新建文件夹「新建文件夹」/ 重命名当前文件名。
    fn default_name(&self) -> String {
        match self {
            FileTreePromptKind::NewFile { .. } => "新建文件.md".to_string(),
            FileTreePromptKind::NewFolder { .. } => "新建文件夹".to_string(),
            FileTreePromptKind::Rename { path, .. } => path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// 文件树输入弹层实体（宿主持有 `Option<Entity<Self>>`；渲染挂根级高优先级层）。
pub(crate) struct FileTreePrompt {
    kind: FileTreePromptKind,
    /// 单行输入框状态（预填默认名；IME/占位符复用）。
    input: gpui::Entity<InputState>,
    /// 宿主（EditorApp）弱引用：提交回调执行文件系统操作与树/选项卡同步。
    host: WeakEntity<EditorApp>,
    /// 最近一次提交失败的内联提示（重名/非法名/权限等；成功后置 None）。
    hint: Option<String>,
    /// 打开前的焦点句柄（关闭时归还；提交成功后文件已打开 → 焦点被动作带走不归还）。
    previous_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

impl FileTreePrompt {
    pub(crate) fn new(
        host: WeakEntity<EditorApp>,
        kind: FileTreePromptKind,
        previous_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default = kind.default_name();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("名称").default_value(default)
        });
        Self {
            kind,
            input,
            host,
            hint: None,
            previous_focus,
            focus_handle: cx.focus_handle(),
        }
    }

    /// 回车 / 点「确定」：读输入 → 回调宿主执行 → 成功关闭 / 失败内联提示不静默。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        let kind = self.kind.clone();
        let Some(host) = self.host.clone().upgrade() else {
            self.close(window, cx);
            return;
        };
        let result = host.update(cx, |app, cx| {
            app.file_tree_submit_prompt(&kind, &value, window, cx)
        });
        match result {
            Ok(()) => self.close(window, cx),
            Err(msg) => {
                self.hint = Some(msg);
                cx.notify();
            }
        }
    }

    /// 关闭：发 `DismissEvent`（宿主清引用）；焦点未被动作带走才归还打开前的焦点。
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus_still_ours = window
            .focused(cx)
            .is_some_and(|f| f == self.focus_handle.clone() || f == self.input_focus(cx));
        cx.stop_propagation();
        window.prevent_default();
        cx.emit(DismissEvent);
        if focus_still_ours
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    /// 输入框焦点句柄（宿主打开时聚焦 + 关闭判定共用）。
    pub(crate) fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_ref() {
            "enter" => {
                cx.stop_propagation();
                window.prevent_default();
                self.submit(window, cx);
            }
            "escape" => {
                cx.stop_propagation();
                self.close(window, cx);
            }
            _ => {}
        }
    }

    fn on_mouse_down_out(
        &mut self,
        _e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close(window, cx);
    }

    /// 测试可见性。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn kind(&self) -> &FileTreePromptKind {
        &self.kind
    }

    /// 测试可见性（当前输入值）。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn input_value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// 测试可见性（输入框实体：测试经 `set_value` 程序化改输入）。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn input_entity(&self) -> gpui::Entity<InputState> {
        self.input.clone()
    }
}

impl EventEmitter<DismissEvent> for FileTreePrompt {}

impl Focusable for FileTreePrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileTreePrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 开卡即聚焦输入框（回车/直接敲字直达）。每帧重夺焦点：右键菜单关闭时会按
        // `spawn_context_menu` 的 previous_focus 归还旧焦点（如刚打开的文件编辑器），
        // 若不重夺，输入框会失焦、回车进不了 submit（真机同隐患）。
        let input_focus = self.input_focus(cx);
        if !input_focus.is_focused(window) {
            window.focus(&input_focus, cx);
        }
        let theme = ui::theme::Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        let surface: gpui::Hsla = colors.surface.into();
        let fg: gpui::Hsla = colors.fg.into();
        let muted: gpui::Hsla = colors.fg_muted.into();
        let accent: gpui::Hsla = colors.accent.into();
        let error: gpui::Hsla = colors.error.into();

        let title = self.kind.title();
        let hint = self.hint.clone();
        // 聚焦时输入框边框点亮为强调色（与侧栏搜索框同款）。
        let input_focused = self.input_focus(cx).is_focused(window);
        let border_color: gpui::Hsla = if input_focused {
            colors.focus_ring.into()
        } else {
            colors.border.into()
        };
        // 按钮：取消 = 次要灰字；确定 = 强调色字（主题色，无硬编码）。
        // 点击确定 → 提交（与回车同语义）；点击取消 → 关闭（与 Esc 同语义）。
        let ok_btn = div()
            .id("file-tree-prompt-ok")
            .debug_selector(|| "file-tree-prompt-ok".into())
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(12.))
            .text_color(accent)
            .cursor(CursorStyle::PointingHand)
            .on_click(cx.listener(move |this: &mut FileTreePrompt, _ev, window, cx| {
                this.submit(window, cx);
            }))
            .child("确定")
            .into_any_element();
        let cancel_btn = div()
            .id("file-tree-prompt-cancel")
            .debug_selector(|| "file-tree-prompt-cancel".into())
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(12.))
            .text_color(muted)
            .cursor(CursorStyle::PointingHand)
            .on_click(cx.listener(move |this: &mut FileTreePrompt, _ev, window, cx| {
                this.close(window, cx);
            }))
            .child("取消")
            .into_any_element();
        let card = div()
            .id("file-tree-prompt")
            .debug_selector(|| "file-tree-prompt".into())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .flex()
            .flex_col()
            .w(px(360.))
            .p(px(16.))
            .gap_y(px(12.))
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(surface.opacity(if is_dark {
                ui::theme::FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT
            }))
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.20))
                    .blur_radius(px(14.)),
                gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                    .blur_radius(px(4.)),
            ])
            .with_spring(
                "file-tree-prompt-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(title),
            )
            .child(
                // Fluent 文本框：整圈细边框 + 内缩边距。
                div()
                    .id("file-tree-prompt-input")
                    .debug_selector(|| "file-tree-prompt-input".into())
                    .px_2()
                    .py_1()
                    .w_full()
                    .rounded_sm()
                    .border_1()
                    .border_color(border_color)
                    .child(Input::new(&self.input)),
            )
            .children(hint.map(|h| {
                div()
                    .id("file-tree-prompt-hint")
                    .debug_selector(|| "file-tree-prompt-hint".into())
                    .text_size(px(11.))
                    .text_color(error)
                    .child(h)
                    .into_any_element()
            }))
            .child(
                div()
                    .id("file-tree-prompt-buttons")
                    .debug_selector(|| "file-tree-prompt-buttons".into())
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(cancel_btn)
                    .child(ok_btn),
            );

        // 合成层纪律（同 RecentPanel/命令面板）：整层 deferred 独立绘制 + 全窗包裹
        // 做水平居中 + `occlude()`（BlockMouse）防点击穿透到下层。
        let win_w = window.bounds().size.width.as_f32();
        let x = ((win_w - 360.) / 2.).max(8.);
        deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .child(div().absolute().left(px(x)).top(px(120.)).child(card)),
        )
        .with_priority(220)
    }
}
