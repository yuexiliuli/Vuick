//! 自绘通知：`Notification` 构建器 + 卡片渲染，替代
//! `gpui_component::notification::{Notification, NotificationType}`。
//!
//! # 存储与生命周期
//! 通知**容器与计时由调用方（App）自持**：`EditorApp::notifications` 保存 `Notification` 数据，
//! 渲染时经 [`Notification::render_card`] 产出卡片；点击关闭 / 定时移除走 App 自己的
//! `remove_notification1::<T>(key)`（自托管 `spawn` 计时，不依赖通知层窗口激活倒计时）。
//! 这样组件与 gpui-component 的 `Root`/`WindowExt` 完全解耦（#9/#11 落地即彻底断开；
//! `WindowExt` 现已确认全仓零引用，随 #9 一并关闭）。

use std::any::TypeId;

use gpui::{
    App, ElementId, Hsla, InteractiveElement, ParentElement, SharedString, Stateful, Styled, Window,
    div, prelude::FluentBuilder as _, px,
};

use crate::ui::theme::{ColorScheme, Theme};

/// 通知类型（决定卡片左侧色点）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotificationKind {
    /// 错误/失败。
    Error,
    /// 普通信息。
    #[allow(dead_code)] // 语义色出口预留：App 目前只用 Error
    Info,
}

impl NotificationKind {
    /// 类型强调色：全部取 `ColorScheme` 语义字段。
    fn accent(&self, colors: &ColorScheme) -> Hsla {
        match self {
            NotificationKind::Error => colors.error.into(),
            NotificationKind::Info => colors.accent.into(),
        }
    }
}

/// 一条通知数据（构建器模式）。标识 = 类型 `T` + 唯一 `key`，用于精确移除。
#[derive(Clone)]
pub struct Notification {
    id_type: TypeId,
    key: ElementId,
    title: SharedString,
    message: SharedString,
    kind: NotificationKind,
}

impl Notification {
    pub fn new() -> Self {
        Self {
            id_type: TypeId::of::<()>(),
            key: ElementId::from(0usize),
            title: SharedString::default(),
            message: SharedString::default(),
            kind: NotificationKind::Error,
        }
    }

    /// 用类型 + 唯一 key 标识（`remove_notification1::<T>(key)` 精确移除）。
    pub fn id1<T: 'static>(mut self, key: impl Into<ElementId>) -> Self {
        self.id_type = TypeId::of::<T>();
        self.key = key.into();
        self
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = title.into();
        self
    }

    pub fn message(mut self, message: impl Into<SharedString>) -> Self {
        self.message = message.into();
        self
    }

    pub fn with_type(mut self, kind: NotificationKind) -> Self {
        self.kind = kind;
        self
    }

    /// App 自托管计时移除，故接受该项但忽略（保证必然消失）。
    #[allow(dead_code)] // 与 gpui-component 的 API 对齐保留；App 走自托管计时
    pub fn autohide(self, _: bool) -> Self {
        self
    }

    pub fn id_type(&self) -> TypeId {
        self.id_type
    }

    pub fn key(&self) -> &ElementId {
        &self.key
    }

    #[allow(dead_code)] // 公开读取器：测试用；App 现只关心 id_type/key
    pub fn kind(&self) -> NotificationKind {
        self.kind
    }

    /// 卡片外观：左侧类型色点 + 标题 +（可选）多行消息。
    ///
    /// 不含关闭交互——关闭由 App 用 [`Notification::id_type`] + [`Notification::key`] 接线，
    /// 在返回的 `Stateful` 上 `.on_click(...)` 即可。
    pub fn render_card(&self, _window: &mut Window, cx: &mut App) -> Stateful<gpui::Div> {
        let colors = Theme::global(cx).colors;
        let accent = self.kind.accent(&colors);
        let surface: Hsla = colors.surface.into();
        let fg: Hsla = colors.fg.into();
        let border: Hsla = colors.border.into();

        div()
            .id(self.key.clone())
            .cursor_pointer()
            .flex()
            .flex_col()
            .w(px(320.))
            .px_3()
            .py_2()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().rounded_full().w(px(8.)).h(px(8.)).bg(accent))
                    .child(self.title.clone()),
            )
            .when(!self.message.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .text_size(px(13.))
                        .text_color(fg.opacity(0.82))
                        .whitespace_normal()
                        .child(self.message.clone()),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Marker;

    #[test]
    fn builder_sets_fields_and_key_identity() {
        let key = ElementId::from("open-fail-1");
        let n = Notification::new()
            .id1::<Marker>(key.clone())
            .title("打开失败")
            .message("无法读取文件") 
            .with_type(NotificationKind::Error)
            .autohide(false);

        assert_eq!(n.id_type(), TypeId::of::<Marker>());
        assert_eq!(n.key(), &key);
        assert_eq!(n.kind(), NotificationKind::Error);
        assert_eq!(n.title.as_ref(), "打开失败");
        assert_eq!(n.message.as_ref(), "无法读取文件");
    }

    #[test]
    fn distinct_keys_are_distinct_identity() {
        let a = Notification::new().id1::<Marker>(ElementId::from("k1"));
        let b = Notification::new().id1::<Marker>(ElementId::from("k2"));
        assert!(a.key() != b.key());
        assert_eq!(a.id_type(), b.id_type(), "同类型不同 key 应可并存");
    }
}
