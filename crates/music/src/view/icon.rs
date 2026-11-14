//! 音乐播放器自绘图标（自含于本 crate，零依赖）：`[`MusicIcon`]` + [`IconView`]。
//!
//! 实现照抄 `crates/ui/src/icon.rs` 的姿势：每个枚举值对应一段 24×24 的 SVG 标记
//! （`fill/stroke="currentColor"`），gpu `Window::paint_svg` 按 `.text_color()` 上色、
//! 按 `.size()` 缩放（GPUI 自带 SVG 光栅，无新增依赖）。标记复用 Feather 风格路径，
//! play/pause 等与原型 Material Icons 语义一致。

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window, px,
};

use super::theme;

/// 音乐播放器 UI 用到的本地图标全集（Slice2 后仅剩未迁 ui 的 8 项；其余已迁
/// `ui::IconName`，走 `ui::icon::Icon`）。按需扩充，Feather/Material 混合路径。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MusicIcon {
    /// 上一首。
    Prev,
    /// 下一首。
    Next,
    /// 播放（实心三角）。
    Play,
    /// 暂停（双竖条）。
    Pause,
    /// 音量（喇叭 + 声波）。
    Volume,
    /// 窗口最小化（横向短杠，Win11 语义）。
    Minimize,
    /// 窗口最大化（单方块）。
    Maximize,
    /// 窗口向下还原（双错位方块）。
    Restore,
}

impl MusicIcon {
    /// 单一来源的 SVG 标记（24×24 viewBox、Fluent 光学线宽 1.5、round 端点）。
    fn svg_markup(self) -> String {
        const H: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" \
            viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" \
            stroke-linecap=\"round\" stroke-linejoin=\"round\">";
        const T: &str = "</svg>";
        let inner = match self {
            MusicIcon::Prev => {
                "<polygon points=\"19 20 9 12 19 4 19 20\"/><line x1=\"5\" y1=\"19\" x2=\"5\" y2=\"5\"/>"
            }
            MusicIcon::Next => {
                "<polygon points=\"5 4 15 12 5 20 5 4\"/><line x1=\"19\" y1=\"5\" x2=\"19\" y2=\"19\"/>"
            }
            MusicIcon::Play => {
                "<polygon points=\"7 4 20 12 7 20 7 4\" fill=\"currentColor\" stroke=\"currentColor\"/>"
            }
            MusicIcon::Pause => {
                "<line x1=\"7\" y1=\"4\" x2=\"7\" y2=\"20\"/><line x1=\"17\" y1=\"4\" x2=\"17\" y2=\"20\"/>"
            }
            MusicIcon::Volume => {
                "<polygon points=\"11 5 6 9 2 9 2 15 6 15 11 19 11 5\"/><path d=\"M15.5 8.5a5 5 0 0 1 0 7\"/><path d=\"M19 5a10 10 0 0 1 0 14\"/>"
            }
            MusicIcon::Minimize => {
                "<line x1=\"5\" y1=\"12\" x2=\"19\" y2=\"12\"/>"
            }
            MusicIcon::Maximize => {
                "<rect x=\"5\" y=\"5\" width=\"14\" height=\"14\" rx=\"1\"/>"
            }
            // 还原 = 两个错位方块（右下前景 + 左上背景上边/左边），对齐原型 restore-group。
            MusicIcon::Restore => {
                "<rect x=\"9\" y=\"9\" width=\"13\" height=\"13\" rx=\"1.5\"/><path d=\"M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1\"/>"
            }
        };
        format!("{H}{inner}{T}")
    }
}

/// 音乐图标视图：固定尺寸、单一着色（对齐 `ui::icon::Icon` 用法）。
#[derive(Clone)]
pub struct IconView {
    name: MusicIcon,
    size: Pixels,
    color: Hsla,
}

impl IconView {
    pub fn new(name: MusicIcon) -> Self {
        Self {
            name,
            size: px(16.),
            color: theme::text(),
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    pub fn text_color(mut self, color: Hsla) -> Self {
        self.color = color;
        self
    }
}

impl IntoElement for IconView {
    type Element = IconView;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for IconView {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        base_util_source_location()
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = self.size.into();
        style.size.height = self.size.into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let markup = self.name.svg_markup();
        let key = format!("__music_icon__{:?}", self.name).into();
        window
            .paint_svg(
                bounds,
                key,
                Some(markup.as_bytes()),
                Default::default(),
                self.color,
                cx,
            )
            .ok();
    }
}

/// 占位 source_location（对齐 `base::util::source_location` 的兜底；无 base 依赖时给定位）。
fn base_util_source_location() -> Option<&'static std::panic::Location<'static>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_wellformed() {
        let names = [
            MusicIcon::Prev,
            MusicIcon::Next,
            MusicIcon::Play,
            MusicIcon::Pause,
            MusicIcon::Volume,
            MusicIcon::Minimize,
            MusicIcon::Maximize,
            MusicIcon::Restore,
        ];
        for name in names {
            let m = name.svg_markup();
            assert!(m.contains("<svg"), "{name:?} 应产出 svg 标记");
            assert!(m.contains("currentColor"), "{name:?} 应收 currentColor 统一上色");
            assert!(m.ends_with("</svg>"), "{name:?} 应为闭合 svg");
        }
    }
}
