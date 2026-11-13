//! 自绘图标：`IconName` + [`Icon`] 视图，替代 `gpui_component::{Icon, IconName}`。
//!
//! 实现：每个枚举值对应一段 24×24 的 SVG 标记（`fill="currentColor"`/`stroke="currentColor"`），
//! 用 gpui 自带的 SVG 光栅（`Window::paint_svg`，**零新增依赖**）按 `.text_color(...)` 统一
//! 上色、按 `.size(...)` 缩放。标记复用 Feather 风格路径。

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window, px,
};

/// 图标名：本应用实际用到的图标全集（按需扩充）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IconName {
    PanelLeftClose,
    PanelLeftOpen,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    WindowClose,
    BookOpen,
    Search,
    File,
    ChevronRight,
    ChevronDown,
    /// 文件夹（文件树 / 打开文件夹空态 / 活动栏「文件」按钮）。
    Folder,
    /// 展开状态的文件夹（文件树中展开的目录行）。
    FolderOpen,
    /// 首页（房子）。
    Home,
    /// 发现/探索（罗盘）。
    Explore,
    /// 音乐库（书本）。
    Library,
    /// 电台（广播波）。
    Radio,
    /// 收藏（实心心形：fill=currentColor 实心，替代 music 的 filled() 机制）。
    Heart,
    /// 收藏描边（空心心形：fill=none 仅描边）。
    HeartBorder,
    /// 最近播放（时钟）。
    History,
    /// 本地下载（箭头进托盘）。
    Download,
    /// 通知（铃铛）。
    Bell,
    /// 设置（齿轮）。
    Settings,
    /// 音乐符（封面占位）。
    Note,
    /// 随机播放（交叉箭头）。
    Shuffle,
    /// 循环（回路箭头）。
    Repeat,
    /// 队列（列表 + 音符）。
    QueueMusic,
    /// 分享（三点连线）。
    Share,
    /// 歌词/文本（三行对齐线）。
    Lyrics,
    /// 全屏（四角扩张）。
    Fullscreen,
    /// 退出全屏（四角收拢）。
    FullscreenExit,
    /// 加入播放列表（列表 + 加号）。
    PlaylistAdd,
    /// 拒绝/高品质（四角星）。
    Badge,
    /// 黑胶唱片（logo / 封面占位）。
    Disc,
    /// 播放（视频控制条）。
    #[cfg(feature = "video")]
    Play,
    /// 暂停（视频控制条）。
    #[cfg(feature = "video")]
    Pause,
    /// 音量（视频控制条）。
    #[cfg(feature = "video")]
    Volume,
    /// 静音（视频控制条）。
    #[cfg(feature = "video")]
    VolumeX,
    /// 上一支（视频控制条）。
    #[cfg(feature = "video")]
    SkipBack,
    /// 下一支（视频控制条）。
    #[cfg(feature = "video")]
    SkipForward,
    /// 截图当前帧（视频控制条）。
    #[cfg(feature = "video")]
    Camera,
    /// 解码偏好 = 自动（硬解优先）：「芯片」图标，视频控制条解码偏好按钮硬解态。
    #[cfg(feature = "video")]
    DecodeAuto,
    /// 解码偏好 = 强制软解：「滑杆」图标，视频控制条解码偏好按钮软解态。
    #[cfg(feature = "video")]
    DecodeSoft,
}

impl IconName {
    /// 单一来源的 SVG 标记（24×24 viewBox、2px 安全边距，`currentColor` 上色；单色精灵由
    /// `paint_svg` 的 color 参数统一替换，stroke 图标统一 Fluent 规范：光学线宽 1.5、
    /// 端点/拐角圆形、路径坐标整格对齐——活动栏与标题栏按钮共用同一套线性描边风格）。
    fn svg_markup(self) -> String {
        const H: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" \
            viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.5\" \
            stroke-linecap=\"round\" stroke-linejoin=\"round\">";
        const T: &str = "</svg>";
        let inner = match self {
            IconName::PanelLeftClose => {
                "<rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"2\"/><line x1=\"9\" y1=\"3\" x2=\"9\" y2=\"21\"/><path d=\"m13 15-3-3 3-3\"/>"
            }
            IconName::PanelLeftOpen => {
                "<rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"2\"/><line x1=\"9\" y1=\"3\" x2=\"9\" y2=\"21\"/><path d=\"m11 15 3-3-3-3\"/>"
            }
            IconName::WindowMinimize => "<line x1=\"5\" y1=\"19\" x2=\"19\" y2=\"19\"/>",
            IconName::WindowMaximize => "<rect x=\"3\" y=\"4\" width=\"18\" height=\"16\" rx=\"0\"/>",
            IconName::WindowRestore => {
                "<rect x=\"4\" y=\"4\" width=\"12\" height=\"12\" rx=\"0\"/><path d=\"M8 8h12v12H8z\"/>"
            }
            IconName::WindowClose => {
                "<line x1=\"18\" y1=\"6\" x2=\"6\" y2=\"18\"/><line x1=\"6\" y1=\"6\" x2=\"18\" y2=\"18\"/>"
            }
            IconName::BookOpen => {
                "<path d=\"M2 3h6a4 4 0 0 1 4 4v14a3 3 0 0 0-3-3H2z\"/><path d=\"M22 3h-6a4 4 0 0 0-4 4v14a3 3 0 0 1 3-3h7z\"/>"
            }
            // 搜索：圆 + 45° 柄。柄端点由 16.65 收整到 17（整格坐标，Fluent 网格对齐）。
            IconName::Search => {
                "<circle cx=\"11\" cy=\"11\" r=\"7\"/><line x1=\"21\" y1=\"21\" x2=\"17\" y2=\"17\"/>"
            }
            IconName::File => {
                "<path d=\"M13 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z\"/><path d=\"M13 2v7h7\"/>"
            }
            IconName::ChevronRight => "<path d=\"m9 18 6-6-6-6\"/>",
            IconName::ChevronDown => "<path d=\"m6 9 6 6 6-6\"/>",
            // 文件夹：licide 线性描边（顶部拉手带斜角），展开态多一条「开口 + 纸面」折线。
            IconName::Folder => {
                "<path d=\"M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z\"/>"
            }
            IconName::FolderOpen => {
                "<path d=\"m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2\"/>"
            }
            IconName::Home => {
                "<path d=\"M3 10.5 12 3l9 7.5\"/><path d=\"M5 9.5V21h14V9.5\"/><path d=\"M9 21v-6h6v6\"/>"
            }
            IconName::Explore => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><polygon points=\"15.5 8.5 13.9 13.9 8.5 15.5 10.1 10.1 15.5 8.5\"/>"
            }
            IconName::Library => {
                "<path d=\"M4 19.5A2.5 2.5 0 0 1 6.5 17H20\"/><path d=\"M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z\"/><path d=\"M9 7h7M9 11h5\"/>"
            }
            IconName::Radio => {
                "<circle cx=\"12\" cy=\"13\" r=\"3\"/><path d=\"M7.2 8.2a7 7 0 0 0 0 9.6\"/><path d=\"M16.8 8.2a7 7 0 0 1 0 9.6\"/><path d=\"M4.2 5.6a11 11 0 0 0 0 12.8\"/>"
            }
            // 心形两态：Heart=实心（fill=currentColor+stroke，替代 music 的 filled() 机制）、
            // HeartBorder=描边（fill=none 仅 stroke）；同路径不同 fill。
            IconName::Heart => {
                "<path d=\"M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z\" fill=\"currentColor\" stroke=\"currentColor\"/>"
            }
            IconName::HeartBorder => {
                "<path d=\"M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z\"/>"
            }
            IconName::History => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><path d=\"M12 7v5l3 2\"/>"
            }
            IconName::Download => {
                "<path d=\"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4\"/><path d=\"m7 10 5 5 5-5\"/><path d=\"M12 15V3\"/>"
            }
            IconName::Bell => {
                "<path d=\"M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9\"/><path d=\"M13.73 21a2 2 0 0 1-3.46 0\"/>"
            }
            IconName::Settings => {
                "<circle cx=\"12\" cy=\"12\" r=\"3\"/><path d=\"M12 2v3M12 19v3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M2 12h3M19 12h3M4.9 19.1 7 17M17 7l2.1-2.1\"/>"
            }
            IconName::Note => {
                "<path d=\"M9 18V5l12-2v13\"/><circle cx=\"6\" cy=\"18\" r=\"3\"/><circle cx=\"18\" cy=\"16\" r=\"3\"/>"
            }
            IconName::Shuffle => {
                "<path d=\"m16 3 4 4-4 4\"/><path d=\"M20 7H11a6 6 0 0 0-6 6v7\"/><path d=\"m8 21-4-4 4-4\"/><path d=\"M4 17h9a6 6 0 0 0 6-6V4\"/>"
            }
            IconName::Repeat => {
                "<path d=\"m17 2 4 4-4 4\"/><path d=\"M3 11v-1a4 4 0 0 1 4-4h14\"/><path d=\"m7 22-4-4 4-4\"/><path d=\"M21 13v1a4 4 0 0 1-4 4H3\"/>"
            }
            IconName::QueueMusic => {
                "<path d=\"M3 6h13\"/><path d=\"M3 12h13\"/><path d=\"M3 18h7\"/><circle cx=\"19\" cy=\"16\" r=\"3\"/><path d=\"M18 16v-4l2 1\"/>"
            }
            IconName::Share => {
                "<circle cx=\"18\" cy=\"5\" r=\"3\"/><circle cx=\"6\" cy=\"12\" r=\"3\"/><circle cx=\"18\" cy=\"19\" r=\"3\"/><line x1=\"8.6\" y1=\"13.4\" x2=\"15.4\" y2=\"17.4\"/><line x1=\"15.4\" y1=\"6.6\" x2=\"8.6\" y2=\"10.6\"/>"
            }
            IconName::Lyrics => {
                "<path d=\"M3 5h18\"/><path d=\"M3 11h12\"/><path d=\"M3 17h18\"/>"
            }
            IconName::Fullscreen => {
                "<path d=\"M8 3H5a2 2 0 0 0-2 2v3\"/><path d=\"M21 8V5a2 2 0 0 0-2-2h-3\"/><path d=\"M3 16v3a2 2 0 0 0 2 2h3\"/><path d=\"M16 21h3a2 2 0 0 0 2-2v-3\"/>"
            }
            IconName::FullscreenExit => {
                "<path d=\"M8 3v3a2 2 0 0 1-2 2H3\"/><path d=\"M21 8h-3a2 2 0 0 1-2-2V3\"/><path d=\"M3 16h3a2 2 0 0 1 2 2v3\"/><path d=\"M16 21v-3a2 2 0 0 1 2-2h3\"/>"
            }
            IconName::PlaylistAdd => {
                "<path d=\"M3 6h13\"/><path d=\"M3 12h13\"/><path d=\"M3 18h7\"/><line x1=\"19\" y1=\"14\" x2=\"19\" y2=\"20\"/><line x1=\"16\" y1=\"17\" x2=\"22\" y2=\"17\"/>"
            }
            IconName::Badge => {
                "<path d=\"M12 3l1.9 4.9L19 9.6l-4.2 1.8L12 16l-2.8-4.6L5 9.6l5.1-1.7z\"/>"
            }
            IconName::Disc => {
                "<circle cx=\"12\" cy=\"12\" r=\"9\"/><circle cx=\"12\" cy=\"12\" r=\"2\" fill=\"currentColor\" stroke=\"currentColor\"/>"
            }
            // 播放/暂停（Feather 风格实心多边形，stroke 描边统一）
            #[cfg(feature = "video")]
            IconName::Play => "<polygon points=\"6 3 20 12 6 21 6 3\"/>",
            #[cfg(feature = "video")]
            IconName::Pause => {
                "<line x1=\"6\" y1=\"4\" x2=\"6\" y2=\"20\"/><line x1=\"18\" y1=\"4\" x2=\"18\" y2=\"20\"/>"
            }
            // 音量：喇叭 + 声波弧；静音把声波换成「×」
            #[cfg(feature = "video")]
            IconName::Volume => {
                "<polygon points=\"11 5 6 9 2 9 2 15 6 15 11 19 11 5\"/><path d=\"M15.54 8.46a5 5 0 0 1 0 7.07\"/><path d=\"M19.07 4.93a10 10 0 0 1 0 14.14\"/>"
            }
            #[cfg(feature = "video")]
            IconName::VolumeX => {
                "<polygon points=\"11 5 6 9 2 9 2 15 6 15 11 19 11 5\"/><line x1=\"23\" y1=\"9\" x2=\"17\" y2=\"15\"/><line x1=\"17\" y1=\"9\" x2=\"23\" y2=\"15\"/>"
            }
            #[cfg(feature = "video")]
            IconName::SkipBack => {
                "<polygon points=\"19 20 9 12 19 4 19 20\"/><line x1=\"5\" y1=\"19\" x2=\"5\" y2=\"5\"/>"
            }
            #[cfg(feature = "video")]
            IconName::SkipForward => {
                "<polygon points=\"5 4 15 12 5 20 5 4\"/><line x1=\"19\" y1=\"5\" x2=\"19\" y2=\"19\"/>"
            }
            #[cfg(feature = "video")]
            IconName::Camera => {
                "<path d=\"M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z\"/><circle cx=\"12\" cy=\"13\" r=\"4\"/>"
            }
            // 解码偏好按钮两态：自动（硬解优先）= 芯片（GPU 硬件解码的视觉隐喻）；
            // 强制软解 = 滑杆（软件可调参数的隐喻）。两态图标不同，一眼可辨。
            #[cfg(feature = "video")]
            IconName::DecodeAuto => {
                "<rect x=\"4\" y=\"4\" width=\"16\" height=\"16\" rx=\"2\"/><rect x=\"9\" y=\"9\" width=\"6\" height=\"6\"/><line x1=\"9\" y1=\"2\" x2=\"9\" y2=\"4\"/><line x1=\"15\" y1=\"2\" x2=\"15\" y2=\"4\"/><line x1=\"9\" y1=\"20\" x2=\"9\" y2=\"22\"/><line x1=\"15\" y1=\"20\" x2=\"15\" y2=\"22\"/><line x1=\"2\" y1=\"9\" x2=\"4\" y2=\"9\"/><line x1=\"2\" y1=\"15\" x2=\"4\" y2=\"15\"/><line x1=\"20\" y1=\"9\" x2=\"22\" y2=\"9\"/><line x1=\"20\" y1=\"15\" x2=\"22\" y2=\"15\"/>"
            }
            #[cfg(feature = "video")]
            IconName::DecodeSoft => {
                "<line x1=\"4\" y1=\"21\" x2=\"4\" y2=\"14\"/><line x1=\"4\" y1=\"10\" x2=\"4\" y2=\"3\"/><line x1=\"12\" y1=\"21\" x2=\"12\" y2=\"12\"/><line x1=\"12\" y1=\"8\" x2=\"12\" y2=\"3\"/><line x1=\"20\" y1=\"21\" x2=\"20\" y2=\"16\"/><line x1=\"20\" y1=\"12\" x2=\"20\" y2=\"3\"/><line x1=\"1\" y1=\"14\" x2=\"7\" y2=\"14\"/><line x1=\"9\" y1=\"8\" x2=\"15\" y2=\"8\"/><line x1=\"17\" y1=\"16\" x2=\"23\" y2=\"16\"/>"
            }
        };
        format!("{H}{inner}{T}")
    }
}

/// 自绘图标视图：固定尺寸、单一着色。用法与 `gpui_component::Icon` 对齐：
/// `Icon::new(IconName::Search).size(px(22.)).text_color(color)`。
#[derive(Clone)]
pub struct Icon {
    name: IconName,
    size: Pixels,
    color: Hsla,
}

impl Icon {
    pub fn new(name: IconName) -> Self {
        Self {
            name,
            size: px(16.),
            color: gpui::white(),
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

impl IntoElement for Icon {
    type Element = Icon;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Icon {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        base::util::source_location()
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
        let key = format!("__ui_icon__{:?}", self.name).into();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_wellformed_and_tintable() {
        let names = vec![
            IconName::PanelLeftClose,
            IconName::PanelLeftOpen,
            IconName::WindowMinimize,
            IconName::WindowMaximize,
            IconName::WindowRestore,
            IconName::WindowClose,
            IconName::BookOpen,
            IconName::Search,
            IconName::File,
            IconName::ChevronRight,
            IconName::ChevronDown,
            IconName::Folder,
            IconName::FolderOpen,
            IconName::Home,
            IconName::Explore,
            IconName::Library,
            IconName::Radio,
            IconName::Heart,
            IconName::HeartBorder,
            IconName::History,
            IconName::Download,
            IconName::Bell,
            IconName::Settings,
            IconName::Note,
            IconName::Shuffle,
            IconName::Repeat,
            IconName::QueueMusic,
            IconName::Share,
            IconName::Lyrics,
            IconName::Fullscreen,
            IconName::FullscreenExit,
            IconName::PlaylistAdd,
            IconName::Badge,
            IconName::Disc,
            #[cfg(feature = "video")]
            IconName::Play,
            #[cfg(feature = "video")]
            IconName::Pause,
            #[cfg(feature = "video")]
            IconName::Volume,
            #[cfg(feature = "video")]
            IconName::VolumeX,
            #[cfg(feature = "video")]
            IconName::SkipBack,
            #[cfg(feature = "video")]
            IconName::SkipForward,
            #[cfg(feature = "video")]
            IconName::Camera,
            #[cfg(feature = "video")]
            IconName::DecodeAuto,
            #[cfg(feature = "video")]
            IconName::DecodeSoft,
        ];
        for name in names {
            let m = name.svg_markup();
            assert!(m.contains("<svg"), "{:?} 应产出 svg 标记", name);
            assert!(m.contains("currentColor"), "{:?} 应收 currentColor 统一上色", name);
            assert!(m.contains("viewBox=\"0 0 24 24\""), "{:?} 应带 24x24 viewBox", name);
            // Fluent 线性描边规范：光学线宽 1.5 + 端点/拐角圆形（活动栏与标题栏统一）。
            assert!(
                m.contains("stroke-width=\"1.5\""),
                "{name:?} 应统一 1.5px 光学线宽"
            );
            assert!(
                m.contains("stroke-linecap=\"round\"")
                    && m.contains("stroke-linejoin=\"round\""),
                "{name:?} 应按 Fluent 规范圆角端点/拐角"
            );
            assert!(m.ends_with("</svg>"), "{:?} 应为闭合 svg", name);
        }
    }

    /// 核心（活动栏 / 标题栏）图标绘制满足 Fluent 网格规范：所有路径/属性坐标都是
    /// 整数（整格对齐），且几何落点不超出 24×24 视口的 2px 安全边距（≤22，且不留
    /// 0..2 的悬空带）。0/1 为弧侧标记/线旗标（非落点坐标）、负数为相对偏移，不参与
    /// 下界检查。视频控制条图标含曲线小数（音量声波弧等），不在此范围内检查。
    #[test]
    fn core_icons_stay_on_integer_grid_within_margin() {
        let names = [
            IconName::PanelLeftClose,
            IconName::PanelLeftOpen,
            IconName::WindowMinimize,
            IconName::WindowMaximize,
            IconName::WindowRestore,
            IconName::WindowClose,
            IconName::BookOpen,
            IconName::Search,
            IconName::File,
            IconName::ChevronRight,
            IconName::ChevronDown,
        ];
        // 剥掉属性名（x1=/y2=/width=/d=…），只留数值与路径命令字母，避免属性名的
        // 数字（如 x1 里的 1）被当成坐标。
        let strip_labels = regex::Regex::new(r"[A-Za-z]+[0-9]*=").unwrap();
        let nums_re = regex::Regex::new(r"-?(?:\d+(?:\.\d+)?)").unwrap();
        for name in names {
            let m = name.svg_markup();
            // 只取内层绘制（剥掉 svg 开标签与 </svg>），避免 viewBox 的 "24" 误伤边距检查。
            let inner = &m[m.find('>').map(|i| i + 1).unwrap_or(0)
                ..m.rfind("</svg>").unwrap_or(m.len())];
            let stripped = strip_labels.replace_all(inner, "");
            let nums: Vec<f64> = nums_re
                .find_iter(&stripped)
                .map(|cap| cap.as_str().parse::<f64>().expect("坐标应为数字"))
                .collect();
            assert!(!nums.is_empty(), "{name:?} 应含有可解析的路径坐标");
            for n in nums {
                assert!(
                    n.fract() == 0.0,
                    "{name:?} 坐标应整格对齐（Fluent 网格），遇到小数 {n}：{inner}"
                );
                assert!(
                    n <= 22.0,
                    "{name:?} 坐标应落在 2px 安全边距 [2,22] 内（上界），遇到 {n}：{inner}"
                );
                // 下界：几何落点不允许留在 2px 安全边距内（0/1 为弧/线旗标豁免）。
                if n >= 0.0 {
                    assert!(
                        n == 0.0 || n == 1.0 || n >= 2.0,
                        "{name:?} 坐标落在 2px 安全边距的下界带内（0..2），遇到 {n}：{inner}"
                    );
                }
            }
        }
    }
}
