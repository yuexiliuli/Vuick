//! 显示 / 渲染（display/placeholder）子模块：`Render` 上屏、C6「滚动方向最近缓存图
//! 透明垫底预上传」、加载中 / 失败居中占位文本。
//!
//! 几何与状态（适应视口 / 缩放 / 平移 clamp、展示像素需要刷新）方法留在 [`super`]；本子
//! 模块只拼 element 树，经 `impl ImageViewer` 与 `impl Render` 挂到类型上。

use std::path::Path;

use gpui::{
    div, img, px, Context, CursorStyle, Hsla, InteractiveElement, IntoElement, MouseButton,
    ObjectFit, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, StyledImage,
    Window,
};

use super::ImageViewer;

impl ImageViewer {

    /// C6：取滚动方向最近「已解码在缓存、但非当前显示」的最多 3 张图，各自包成**垫底透明
    /// img**（同尺寸、opacity 0、不拦截输入）叠进返回值。让 gpui 在上屏前就完成这些纹理的
    /// GPU 上传，切到的那一帧不再肩负首次上传（真实 GPU 生效；headless 测试无真实纹理，
    /// 仅保证构造正确）。前向加深到 3 张：快翻时后 2 张大概率已缓存，一次预上传 3 张省
    /// 掉连续切换里「首次上传」的卡顿；缓存没填满 3 张就只叠加存在者，重复路径去重。
    fn peek_upgrade_elements(&self) -> Vec<gpui::AnyElement> {
        let n = self.nav.len() as i64;
        if n <= 1 {
            return Vec::new();
        }
        let mut seen: Vec<&Path> = Vec::new();
        let mut out = Vec::new();
        for k in 1i64..=3 {
            let ni = (self.nav.current() as i64 + self.prefetch_dir as i64 * k).rem_euclid(n) as usize;
            let Some(target) = self.nav.get(ni) else {
                continue;
            };
            if *target == self.path || seen.iter().any(|p| *p == target) {
                continue;
            }
            let Some(entry) = self.cache.iter().find(|e| e.path == *target) else {
                continue;
            };
            seen.push(target);
            let render = entry.render.clone();
            out.push(
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .w_full()
                    .h_full()
                    .opacity(0.0)
                    .child(
                        img(gpui::ImageSource::Render(render))
                            .object_fit(ObjectFit::Contain)
                            .w_full()
                            .h_full(),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// 居中占位文本（加载中 / 加载失败共用）。
    fn placeholder(text: &str, color: Hsla) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.))
            .text_color(color)
            .child(text.to_string())
            .into_any_element()
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ui::theme::Theme::global(cx);
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let muted: Hsla = theme.colors.fg_muted.into();
        let surface: Hsla = theme.colors.surface.into();
        // 记录可视视口并刷新「展示像素需要」（JPEG 解码档位依据；缩放事件期也刷新）
        self.last_viewport = self.viewport_size(window);
        self.update_display_need();
        let (vw, vh) = self.last_viewport;

        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开图片：{err}"), muted)
        } else if let (Some(render), Some((iw, ih))) = (self.image.clone(), self.intrinsic) {
            let (dw, dh) = self.display_size(iw, ih, vw, vh);
            // 放大后平移：clamp 到图始终覆盖视口（未放大时 pan 恒 0 = 居中；中心锚定缩放不变）
            self.pan = Self::clamp_pan(self.pan, vw, vh, dw, dh);
            let ox = (vw - dw) * 0.5 + self.pan.0;
            let oy = (vh - dh) * 0.5 + self.pan.1;
            // 动图由本查看器时钟逐帧切 `self.image`（单帧 RenderImage）驱动（见
            // [`Self::spawn_anim_clock`]）；此处直接画当前帧，静态图同理，无需 element id。
            img(gpui::ImageSource::Render(render))
                .object_fit(ObjectFit::Contain)
                .absolute()
                .left(px(ox))
                .top(px(oy))
                .w(px(dw))
                .h(px(dh))
                .into_any_element()
        } else {
            Self::placeholder("正在加载图片…", muted)
        };

        // 底部信息条：第几张/共几张 · 缩放百分比（含锁定模式；名称不显示）。
        let info = SharedString::from(format!(
            "{}/{} · {:.0}%",
            self.nav.current() + 1,
            self.nav.len(),
            self.zoom * 100.0
        ));

        // C6：把方向最近的已缓存图（最多 3 张）以透明 img 垫底，纹理预先上传（详见
        // peek_upgrade_elements）
        let preheat = self.peek_upgrade_elements();

        let mut root = div()
            .id("image-viewer")
            .size_full()
            .relative()
            .bg(canvas);
        for peek in preheat {
            root = root.child(peek);
        }
        root = root
            .child(
                div()
                    .id("image-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .track_scroll(&self.viewport)
                    .child(
                        div()
                            .id("image-wrap")
                            .size_full()
                            .relative()
                            .cursor(if self.drag.is_some() {
                                CursorStyle::ClosedHand
                            } else {
                                CursorStyle::OpenHand
                            })
                            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_pan_down))
                            .on_mouse_move(cx.listener(Self::on_pan_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_pan_up))
                            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_pan_up))
                            .child(body),
                    ),
            );
        // 底部信息条：第几张/共几张 · 缩放百分比。普通模式常驻；锁定模式按
        // `info_auto.visible()`（切图/悬停浮现，1s 自隐）。
        if !self.locked || self.info_auto.visible() {
            root = root.child(
                div()
                    .id("image-info")
                    .absolute()
                    .left(px(12.))
                    .bottom(px(12.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(surface)
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(info),
            );
        }
        // 锁定模式：左下信息热区（透明、盖在信息条之上）——鼠标移入即浮现、移开 1s 后隐藏
        if self.locked {
            root = root.child(
                div()
                    .id("image-info-zone")
                    .absolute()
                    .left(px(0.))
                    .bottom(px(0.))
                    .w(px(260.))
                    .h(px(52.))
                    .on_hover(cx.listener(Self::on_info_zone_hover)),
            );
        }
        root
    }
}
