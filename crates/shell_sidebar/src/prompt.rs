//! 文件树「新建/重命名」输入弹层。
//!
//! 居中 Acrylic 卡片 + 单行输入框（复用 `ui::input`），回车/确定提交 → 回调宿主执行
//! 文件系统操作；失败在卡片内联提示（不静默丢反馈）；Esc/取消/点外 → 关闭（发
//! `DismissEvent`，宿主清引用释放实体）。焦点管理同 `session_store::RecentPanel`
//! （开前记焦点、动作未带走才归还）。
//!
//! 泛型宿主 [`FileTreePromptHost`]：域 crate 不依赖 shell
//! 具体宿主类型，`WeakEntity<H>` 弱引用反调提交动作。

use std::path::PathBuf;

use gpui::{
    div, px, AnimationExt, App, AppContext, Context, DismissEvent, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
};
use ui::button::ButtonVariants as _;
use ui::float_card::{FloatCardBg, FloatCardPad, FloatCardRadius, FloatCardShadow};
use ui::input::{Input, InputEvent, InputState};
use ui::motion::hover_spring;

/// 文件树输入弹层的宿主抽象（提交动作在宿主：文件系统操作 + 树/选项卡同步）。
pub trait FileTreePromptHost: 'static {
    /// 提交：读输入值 → 按 `kind` 执行新建/重命名文件系统操作；成功 `Ok(())`，失败
    /// `Err(内联提示文案)`（弹层留在卡片上显式展示，不静默丢反馈）。宿主内部完成
    /// 提交后「成功才关、失败留卡」由弹层依返回值处理。
    fn file_tree_submit_prompt(
        &mut self,
        kind: &FileTreePromptKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String>
    where
        Self: Sized;
}

/// 弹层操作类型（决定标题 / 预填名 / 提交后动作）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileTreePromptKind {
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
/// 泛型参数 `H` = 宿主实体类型（`impl FileTreePromptHost`），经弱引用反调。
pub struct FileTreePrompt<H: FileTreePromptHost> {
    kind: FileTreePromptKind,
    /// 单行输入框状态（预填默认名；IME/占位符复用）。
    input: gpui::Entity<InputState>,
    /// 输入框事件订阅（`Submit`/`Cancel` 上抛 → 提交/关闭；`Change` 无消费）。
    _input_sub: gpui::Subscription,
    /// 宿主（泛型 `H`）弱引用：提交回调执行文件系统操作与树/选项卡同步。
    host: WeakEntity<H>,
    /// 最近一次提交失败的内联提示（重名/非法名/权限等；成功后置 None）。
    hint: Option<String>,
    /// 打开前的焦点句柄（关闭时归还；提交成功后文件已打开 → 焦点被动作带走不归还）。
    previous_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

impl<H: FileTreePromptHost> FileTreePrompt<H> {
    pub fn new(
        host: WeakEntity<H>,
        kind: FileTreePromptKind,
        previous_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default = kind.default_name();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("名称").default_value(default)
        });
        // 订阅输入框事件：Enter（上抛 Submit）→ 提交；Esc（上抛 Cancel）→ 关闭。
        // up 下取迁移：原在卡片 `on_key_down` 手工 parse "enter"/"escape"，现收敛到
        // 输入组件统一事件（行为零变化）。
        let weak_self = cx.weak_entity();
        let _input_sub = window.subscribe(&input, cx, move |_, ev: &InputEvent, window, cx| {
            let Some(this) = weak_self.upgrade() else {
                return;
            };
            let _ = this.update(cx, |this, cx| match ev {
                InputEvent::Submit { .. } => this.submit(window, cx),
                InputEvent::Cancel => this.close(window, cx),
                InputEvent::Change => {}
            });
        });
        Self {
            kind,
            input,
            _input_sub,
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
        let result = host.update(cx, |host, cx| {
            host.file_tree_submit_prompt(&kind, &value, window, cx)
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
    pub fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// 测试可见性（shell 的 `app/tests/sidebar.rs` 跨 crate 断言用）。
    #[doc(hidden)]
    pub fn kind(&self) -> &FileTreePromptKind {
        &self.kind
    }

    /// 测试可见性（当前输入值）。
    #[doc(hidden)]
    pub fn input_value(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// 测试可见性（输入框实体：测试经 `set_value` 程序化改输入）。
    #[doc(hidden)]
    pub fn input_entity(&self) -> gpui::Entity<InputState> {
        self.input.clone()
    }

    fn on_mouse_down_out(
        &mut self,
        _e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close(window, cx);
    }
}

impl<H: FileTreePromptHost> EventEmitter<DismissEvent> for FileTreePrompt<H> {}

impl<H: FileTreePromptHost> Focusable for FileTreePrompt<H> {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<H: FileTreePromptHost> Render for FileTreePrompt<H> {
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
        let fg: gpui::Hsla = colors.fg.into();
        let muted: gpui::Hsla = colors.fg_muted.into();
        let accent: gpui::Hsla = colors.accent.into();
        let error: gpui::Hsla = colors.error.into();

        let title = self.kind.title();
        let hint = self.hint.clone();
        // 聚焦时输入框边框点亮为强调色（聚焦边框色经 focus_border_color 统一收敛，
        // 与侧栏搜索框同款 Fluent 视觉）。
        let border_color = ui::focus_border_color(window, &input_focus, colors.border.into(), cx);
        // 按钮：取消 = 次要灰字；确定 = 强调色字（主题色，无硬编码）。
        // 点击确定 → 提交（与回车同语义）；点击取消 → 关闭（与 Esc 同语义）。
        // 复用 ui::Button::Text 变体：`px_3/py_1/rounded_md/text_size(12)` 走 Styled 链保留
        // 现状几何；`text_color` 沿用 accent/muted 覆盖 Text 默认 fg；`tab_stop(false)` 保持
        // 旧裸 div 的「不可 Tab 聚焦/无键盘激活」焦点语义（焦点在输入框，回车直达提交）。
        let ok_btn = ui::button::Button::new("file-tree-prompt-ok")
            .debug_selector(|| "file-tree-prompt-ok".into())
            .text()
            .label("确定")
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(12.))
            .text_color(accent)
            .tab_stop(false)
            .on_click(cx.listener(move |this: &mut Self, _ev, window, cx| {
                this.submit(window, cx);
            }))
            .into_any_element();
        let cancel_btn = ui::button::Button::new("file-tree-prompt-cancel")
            .debug_selector(|| "file-tree-prompt-cancel".into())
            .text()
            .label("取消")
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(12.))
            .text_color(muted)
            .tab_stop(false)
            .on_click(cx.listener(move |this: &mut Self, _ev, window, cx| {
                this.close(window, cx);
            }))
            .into_any_element();
        let card = ui::float_card(
            &colors,
            is_dark,
            FloatCardBg::Acrylic,
            FloatCardShadow::Double,
            FloatCardRadius::Lg,
            FloatCardPad::P(px(16.), px(12.)),
        )
        .id("file-tree-prompt")
        .debug_selector(|| "file-tree-prompt".into())
        .track_focus(&self.focus_handle)
        .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
        .w(px(360.))
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

        // 合成层约束：整层 deferred 独立绘制 + 全窗包裹
        // 做水平居中 + `occlude()`（BlockMouse）防点击穿透到下层。
        let win_w = window.bounds().size.width.as_f32();
        ui::float_card::overlay_centered(card.into_any_element(), win_w, 360., 120., 220)
    }
}
