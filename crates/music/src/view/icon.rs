//! 音乐播放器自绘图标（自含于本 crate，零依赖）：`[`MusicIcon`]` + [`IconView`]。
//!
//! 实现照抄 `crates/ui/src/icon.rs` 的姿势：每个枚举值对应一段 24×24 的 SVG 标记
//! （`fill/stroke="currentColor"`），gpu `Window::paint_svg` 按 `.text_color()` 上色、
//! 按 `.size()` 缩放（GPUI 自带 SVG 光栅，无新增依赖）。标记复用 Feather 风格路径，
//! play/pause/shuffle/repeat/heart 与原型 Material Icons 语义一致。

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window, px,
};

/// 音乐播放器 UI 用到的图标全集（按需扩充，Feather/Material 混合路径）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MusicIcon {
    /// 首页（房子）。
    Home,
    /// 发现/探索（罗盘）。
    Explore,
    /// 音乐库（书本）。
    Library,
    /// 电台（广播波）。
    Radio,
    /// 收藏（实心心形）。
    Heart,
    /// 收藏描边（空心心形）。
    HeartBorder,
    /// 最近播放（时钟）。
    History,
    /// 本地下载（箭头进托盘）。
    Download,
    /// 搜索。
    Search,
    /// 通知（铃铛）。
    Bell,
    /// 设置（齿轮）。
    Settings,
    /// 音乐符（封面占位）。
    Note,
    /// 随机播放（交叉箭头）。
    Shuffle,
    /// 上一首。
    Prev,
    /// 下一首。
    Next,
    /// 播放（实心三角）。
    Play,
    /// 暂停（双竖条）。
    Pause,
    /// 循环（回路箭头）。
    Repeat,
    /// 队列（列表 + 音符）。
    QueueMusic,
    /// 分享（三点连线）。
    Share,
    /// 歌词/文本（三行对齐线）。
    Lyrics,
    /// 音量（喇叭 + 声波）。
    Volume,
    /// 全屏（四角扩张）。
    Fullscreen,
    /// 退出全屏（四角收拢）。
    FullscreenExit,
    /// 加入播放列表（列表 + 加号）。
    PlaylistAdd,
    /// 更多（横三点）。
    More,
    /// 拒绝/高品质（四角星）。
    Badge,
    /// 黑胶唱片（logo / 封面占位）。
    Disc,
}

impl MusicIcon {
    /// 单一来源的 SVG 标记（24×24 viewBox、Fluent 光学线宽 1.5、round 端点）。
    fn svg_markup(self) -> String {
        const H: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" \
            viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" \
            stroke-linecap=\"round\" stroke-linejoin=\"round\">";
        const T: &str = "</svg>";
        let inner = match self {
            MusicIcon::Home => {
                "<path d=\"M3 10.5 12 3l9 7.5\"/><path d=\"M5 9.5V21h14V9.5\"/><path d=\"M9 21v-6h6v6\"/>"
            }
            MusicIcon::Explore => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><polygon points=\"15.5 8.5 13.9 13.9 8.5 15.5 10.1 10.1 15.5 8.5\"/>"
            }
            MusicIcon::Library => {
                "<path d=\"M4 19.5A2.5 2.5 0 0 1 6.5 17H20\"/><path d=\"M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z\"/><path d=\"M9 7h7M9 11h5\"/>"
            }
            MusicIcon::Radio => {
                "<circle cx=\"12\" cy=\"13\" r=\"3\"/><path d=\"M7.2 8.2a7 7 0 0 0 0 9.6\"/><path d=\"M16.8 8.2a7 7 0 0 1 0 9.6\"/><path d=\"M4.2 5.6a11 11 0 0 0 0 12.8\"/>"
            }
            // 心形：实心（fill=currentColor+stroke）与描边（仅 stroke）同路径。
            MusicIcon::Heart | MusicIcon::HeartBorder => {
                "<path d=\"M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z\"/>"
            }
            MusicIcon::History => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><path d=\"M12 7v5l3 2\"/>"
            }
            MusicIcon::Download => {
                "<path d=\"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4\"/><path d=\"m7 10 5 5 5-5\"/><path d=\"M12 15V3\"/>"
            }
            MusicIcon::Search => {
                "<circle cx=\"11\" cy=\"11\" r=\"7\"/><line x1=\"21\" y1=\"21\" x2=\"17\" y2=\"17\"/>"
            }
            MusicIcon::Bell => {
                "<path d=\"M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9\"/><path d=\"M13.73 21a2 2 0 0 1-3.46 0\"/>"
            }
            MusicIcon::Settings => {
                "<circle cx=\"12\" cy=\"12\" r=\"3\"/><path d=\"M12 2v3M12 19v3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M2 12h3M19 12h3M4.9 19.1 7 17M17 7l2.1-2.1\"/>"
            }
            MusicIcon::Note => {
                "<path d=\"M9 18V5l12-2v13\"/><circle cx=\"6\" cy=\"18\" r=\"3\"/><circle cx=\"18\" cy=\"16\" r=\"3\"/>"
            }
            MusicIcon::Shuffle => {
                "<path d=\"m16 3 4 4-4 4\"/><path d=\"M20 7H11a6 6 0 0 0-6 6v7\"/><path d=\"m8 21-4-4 4-4\"/><path d=\"M4 17h9a6 6 0 0 0 6-6V4\"/>"
            }
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
            MusicIcon::Repeat => {
                "<path d=\"m17 2 4 4-4 4\"/><path d=\"M3 11v-1a4 4 0 0 1 4-4h14\"/><path d=\"m7 22-4-4 4-4\"/><path d=\"M21 13v1a4 4 0 0 1-4 4H3\"/>"
            }
            MusicIcon::QueueMusic => {
                "<path d=\"M3 6h13\"/><path d=\"M3 12h13\"/><path d=\"M3 18h7\"/><circle cx=\"19\" cy=\"16\" r=\"3\"/><path d=\"M18 16v-4l2 1\"/>"
            }
            MusicIcon::Share => {
                "<circle cx=\"18\" cy=\"5\" r=\"3\"/><circle cx=\"6\" cy=\"12\" r=\"3\"/><circle cx=\"18\" cy=\"19\" r=\"3\"/><line x1=\"8.6\" y1=\"13.4\" x2=\"15.4\" y2=\"17.4\"/><line x1=\"15.4\" y1=\"6.6\" x2=\"8.6\" y2=\"10.6\"/>"
            }
            MusicIcon::Lyrics => {
                "<path d=\"M3 5h18\"/><path d=\"M3 11h12\"/><path d=\"M3 17h18\"/>"
            }
            MusicIcon::Volume => {
                "<polygon points=\"11 5 6 9 2 9 2 15 6 15 11 19 11 5\"/><path d=\"M15.5 8.5a5 5 0 0 1 0 7\"/><path d=\"M19 5a10 10 0 0 1 0 14\"/>"
            }
            MusicIcon::Fullscreen => {
                "<path d=\"M8 3H5a2 2 0 0 0-2 2v3\"/><path d=\"M21 8V5a2 2 0 0 0-2-2h-3\"/><path d=\"M3 16v3a2 2 0 0 0 2 2h3\"/><path d=\"M16 21h3a2 2 0 0 0 2-2v-3\"/>"
            }
            MusicIcon::FullscreenExit => {
                "<path d=\"M8 3v3a2 2 0 0 1-2 2H3\"/><path d=\"M21 8h-3a2 2 0 0 1-2-2V3\"/><path d=\"M3 16h3a2 2 0 0 1 2 2v3\"/><path d=\"M16 21v-3a2 2 0 0 1 2-2h3\"/>"
            }
            MusicIcon::PlaylistAdd => {
                "<path d=\"M3 6h13\"/><path d=\"M3 12h13\"/><path d=\"M3 18h7\"/><line x1=\"19\" y1=\"14\" x2=\"19\" y2=\"20\"/><line x1=\"16\" y1=\"17\" x2=\"22\" y2=\"17\"/>"
            }
            MusicIcon::More => {
                "<circle cx=\"5\" cy=\"12\" r=\"1\"/><circle cx=\"12\" cy=\"12\" r=\"1\"/><circle cx=\"19\" cy=\"12\" r=\"1\"/>"
            }
            MusicIcon::Badge => {
                "<path d=\"M12 3l1.9 4.9L19 9.6l-4.2 1.8L12 16l-2.8-4.6L5 9.6l5.1-1.7z\"/>"
            }
            MusicIcon::Disc => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><circle cx=\"12\" cy=\"12\" r=\"2\" fill=\"currentColor\" stroke=\"currentColor\"/>"
            }
        };
        format!("{H}{inner}{T}")
    }
}

/// 音乐图标视图：固定尺寸、单一着色（对齐 `ui::icon::Icon` 用法）。
#[derive(Clone)]
pub struct IconView {
    name: MusicIcon,
    filled: bool,
    size: Pixels,
    color: Hsla,
}

impl IconView {
    pub fn new(name: MusicIcon) -> Self {
        Self {
            name,
            filled: false,
            size: px(16.),
            color: gpui::white(),
        }
    }

    /// 实心变体（心形/播放/黑胶等）。
    pub fn filled(mut self, filled: bool) -> Self {
        self.filled = filled;
        self
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
        let markup = if self.filled && matches!(self.name, MusicIcon::Heart) {
            // 实心心形：fill=currentColor（描边心形无 fill 前缀；其余图标保持原标记）。
            let m = self.name.svg_markup().replace(
                "fill=\"none\" stroke=\"currentColor\"",
                "fill=\"currentColor\" stroke=\"currentColor\"",
            );
            m
        } else {
            self.name.svg_markup()
        };
        let key = format!("__music_icon__{:?}_{}", self.name, self.filled).into();
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
            MusicIcon::Home,
            MusicIcon::Explore,
            MusicIcon::Library,
            MusicIcon::Radio,
            MusicIcon::Heart,
            MusicIcon::HeartBorder,
            MusicIcon::History,
            MusicIcon::Download,
            MusicIcon::Search,
            MusicIcon::Bell,
            MusicIcon::Settings,
            MusicIcon::Note,
            MusicIcon::Shuffle,
            MusicIcon::Prev,
            MusicIcon::Next,
            MusicIcon::Play,
            MusicIcon::Pause,
            MusicIcon::Repeat,
            MusicIcon::QueueMusic,
            MusicIcon::Share,
            MusicIcon::Lyrics,
            MusicIcon::Volume,
            MusicIcon::Fullscreen,
            MusicIcon::FullscreenExit,
            MusicIcon::PlaylistAdd,
            MusicIcon::More,
            MusicIcon::Badge,
            MusicIcon::Disc,
        ];
        for name in names {
            let m = name.svg_markup();
            assert!(m.contains("<svg"), "{name:?} 应产出 svg 标记");
            assert!(m.contains("currentColor"), "{name:?} 应收 currentColor 统一上色");
            assert!(m.ends_with("</svg>"), "{name:?} 应为闭合 svg");
        }
    }
}
