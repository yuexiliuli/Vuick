//! 图片查看器视图：自包含的图片浏览组件（`ImageViewer`）。
//!
//! 设计（对齐 [`crate::pdf::PdfViewer`] 的「自包含阅读器」姿势）：
//! - **打开即显**：`image` crate 在后台线程解码（不阻塞 UI），主线程把 **BGRA** 像素
//!   （GPUI 渲染格式）包成 [`gpui::RenderImage`] 上屏；
//! - **单滚轮切换**：打开时同步扫描**同文件夹**的全部图片（扩展名过滤、文件名升序），
//!   此后单滚轮在清单内循环切换（向下滚 → 下一张，向上滚 → 上一张）；
//! - **Ctrl+滚轮缩放**：缩放倍率作用于「适应视口（Contain）」的基准尺寸，图片始终
//!   水平居中；缩放锚点为图像中心；
//! - **加载守卫**：每次切换自增代序号，只有最新一代解密结果允许落盘（快速连滚时
//!   旧在途解码直接丢弃，不会覆盖新图）；
//! - **通知宿主**：`notify()` + `refresh_windows()` 双保险（实体未注册 window
//!   invalidator 时仅 notify 是空操作），宿主 `EditorApp` 据此同步标签标题与状态栏。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, img, px, rgb, App, Context, Hsla, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, Render, RenderImage, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Window,
};
use image::{Frame, RgbaImage};

/// 缩放下限 / 上限 / 步进（语义同 PDF 阅读器 Ctrl+滚轮缩放，区间更宽以支持图片 1:1 微调）。
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 8.0;
const ZOOM_STEP: f32 = 0.12;
/// 画布四边留白（px，适应视口时的外衬）。
const PAD: f32 = 24.0;

/// 图片查看器：自包含的图片浏览视图。
pub struct ImageViewer {
    /// 当前显示的图片路径（标签标题 / 状态栏由此派生）。
    path: PathBuf,
    /// 已解码的图片（`Some` = 可上屏；切换加载期间保留旧图避免闪白）。
    image: Option<Arc<RenderImage>>,
    /// 图片固有像素尺寸（显示比例依据）。
    intrinsic: Option<(f32, f32)>,
    /// 同文件夹图片清单（文件名升序，始终含当前图，恒非空）。
    folder: Vec<PathBuf>,
    /// 当前图片在 `folder` 中的下标（0 基，取模循环）。
    current: usize,
    /// 缩放倍数（1.0 = 适应视口；Ctrl+滚轮调）。
    zoom: f32,
    /// 是否仍在后台解码（首张或切换中）。
    loading: bool,
    /// 加载失败信息（`Some` = 显示错误占位）。
    load_error: Option<String>,
    /// 视口测量句柄：`#image-scroll`（`overflow_*_scroll` + `track_scroll`）布局后
    /// 经 `bounds()` 给出真实可视区尺寸（首帧回退窗口尺寸）。
    viewport: ScrollHandle,
    /// 画布背景（None → 主题推导；宿主 `EditorApp` 通常显式设置）。
    bg: Option<Hsla>,
    /// 解码代序号：每次 `load_path` 自增；只允许最新代结果落盘。
    load_seq: u64,
}

impl ImageViewer {
    /// 创建查看器并异步加载 `path`（解码 + 同文件夹扫描在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let this = Self {
            path: path.clone(),
            image: None,
            intrinsic: None,
            folder: vec![path.clone()],
            current: 0,
            zoom: 1.0,
            loading: true,
            load_error: None,
            viewport: ScrollHandle::new(),
            bg: None,
            load_seq: 0,
        };
        let scan_at = path.clone();
        let decode_at = path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let decoded = decode_image_at(&decode_at);
                    let folder = scan_folder(&scan_at);
                    (decoded, folder)
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                let (decoded, (folder, current)) = result;
                this.folder = folder;
                this.current = current;
                match decoded {
                    Ok((bgra, w, h)) => {
                        this.install_image(this.path.clone(), bgra, w, h, cx);
                    }
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.notify();
                        cx.refresh_windows();
                    }
                }
            });
        })
        .detach();
        this
    }

    /// 当前图片路径（宿主 `EditorApp` 同步标签标题 / 状态栏用）。
    pub fn current_path(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    /// 当前图片在同文件夹清单中的下标（0 基）。
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// 同文件夹图片总数。
    pub fn folder_len(&self) -> usize {
        self.folder.len()
    }

    /// 是否已加载完成（首张解码成功）。
    #[allow(dead_code)]
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.image.is_some()
    }

    /// 设置画布背景色（宿主按主题传入）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 释放已上屏的 GPU 纹理（关闭标签页时调用，与 PDF 阅读器同款）。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(img) = self.image.take() {
            cx.drop_image(img, Some(window));
        }
    }

    /// 视口尺寸：`#image-scroll` 布局后的真实可视区；首帧未布局（bounds 为 0）
    /// 回退到窗口尺寸（对齐 PDF `wrap_width` 的既有做法）。
    fn viewport_size(&self, window: &Window) -> (f32, f32) {
        let s = self.viewport.bounds().size;
        let w = if s.width.as_f32() > 0.0 {
            s.width.as_f32()
        } else {
            window.bounds().size.width.as_f32()
        };
        let h = if s.height.as_f32() > 0.0 {
            s.height.as_f32()
        } else {
            window.bounds().size.height.as_f32()
        };
        (w, h)
    }

    /// 显示尺寸 = 适应视口（Contain，含 PAD 留白）× 缩放倍数。
    fn display_size(&self, iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        let avail_w = (vw - 2.0 * PAD).max(16.0);
        let avail_h = (vh - 2.0 * PAD).max(16.0);
        let fit = (avail_w / iw).min(avail_h / ih).max(0.001);
        let scale = fit * self.zoom;
        (iw * scale, ih * scale)
    }

    /// 画布默认底色：浅色浅灰 / 深色深灰（与 PDF 阅读器一致）。
    fn default_canvas(cx: &App) -> Hsla {
        if crate::ui::theme::Theme::global(cx).is_dark() {
            rgb(0x111827).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 滚轮处理：Ctrl+滚轮 → 缩放；单滚轮 → 同文件夹切换。挂在内层 wrap
    /// （`#image-scroll` 子树上），bubble 期先于滚动容器原生滚动，`stop_propagation`
    /// 阻止其滚动（对齐 PDF 阅读器的同款姿势）。
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = match event.delta {
            ScrollDelta::Lines(v) => v.y,
            ScrollDelta::Pixels(v) => f32::from(v.y),
        };
        if delta_y == 0.0 {
            return;
        }
        if event.modifiers.control {
            // 实测本平台：向上滚动 delta.y>0 → 放大；向下 <0 → 缩小（与编辑器字号缩放一致）
            let step = delta_y.clamp(-1.0, 1.0);
            self.adjust_zoom(1.0 + step * ZOOM_STEP, cx);
        } else {
            // 单滚轮：向下滚（delta.y<0）→ 下一张；向上滚 → 上一张。
            self.switch(if delta_y < 0.0 { 1 } else { -1 }, cx);
        }
        cx.stop_propagation();
    }

    /// 应用缩放因子并约束在 [ZOOM_MIN, ZOOM_MAX]。
    fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        let new_zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if (new_zoom - self.zoom).abs() < 1e-4 {
            return;
        }
        self.zoom = new_zoom;
        cx.notify();
    }

    /// 在同文件夹清单内切换（`dir` = ±1，取模循环）。
    fn switch(&mut self, dir: i32, cx: &mut Context<Self>) {
        let n = self.folder.len();
        if n <= 1 || dir == 0 {
            return;
        }
        let next = ((self.current as i64 + dir as i64).rem_euclid(n as i64)) as usize;
        if next == self.current {
            return;
        }
        self.current = next;
        let p = self.folder[next].clone();
        self.load_path(p, cx);
    }

    /// 载入指定路径：后台解码（带代序守卫），期间保留旧图避免闪白。
    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.path = path.clone();
        self.loading = true;
        self.load_error = None;
        self.load_seq += 1;
        let seq = self.load_seq;
        let decode_path = path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { decode_image_at(&decode_path) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.load_seq != seq {
                    return; // 已有更新代在途 → 本代结果丢弃
                }
                match result {
                    Ok((bgra, w, h)) => this.install_image(path, bgra, w, h, cx),
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.notify();
                        cx.refresh_windows();
                    }
                }
            });
        })
        .detach();
    }

    /// 解码完成落盘：包 `RenderImage`、复位加载态、通知宿主。
    /// `bgra` 为 **BGRA** 字节序（GPUI 渲染格式；见 [`decode_image_at`]）。
    fn install_image(&mut self, path: PathBuf, bgra: Vec<u8>, w: u32, h: u32, cx: &mut Context<Self>) {
        self.path = path;
        self.loading = false;
        self.load_error = None;
        match RgbaImage::from_raw(w, h, bgra) {
            Some(buf) => {
                let render = Arc::new(RenderImage::new(vec![Frame::new(buf)]));
                self.image = Some(render);
                self.intrinsic = Some((w as f32, h as f32));
            }
            None => {
                self.image = None;
                self.intrinsic = None;
                self.load_error = Some("解码结果尺寸异常".into());
            }
        }
        cx.notify();
        cx.refresh_windows();
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
        let theme = crate::ui::theme::Theme::global(cx);
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let muted: Hsla = theme.colors.fg_muted.into();
        let surface: Hsla = theme.colors.surface.into();
        let (vw, vh) = self.viewport_size(window);

        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            Self::placeholder(&format!("无法打开图片：{err}"), muted)
        } else if let (Some(render), Some((iw, ih))) = (self.image.clone(), self.intrinsic) {
            let (dw, dh) = self.display_size(iw, ih, vw, vh);
            img(gpui::ImageSource::Render(render))
                .object_fit(ObjectFit::Contain)
                .w(px(dw))
                .h(px(dh))
                .into_any_element()
        } else {
            Self::placeholder("正在加载图片…", muted)
        };

        // 底部信息条：文件名 · 第几张/共几张 · 缩放百分比。
        let name = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let info = SharedString::from(format!(
            "{name} · {}/{} · {:.0}%",
            self.current + 1,
            self.folder.len(),
            self.zoom * 100.0
        ));

        div()
            .id("image-viewer")
            .size_full()
            .relative()
            .bg(canvas)
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
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                            .child(body),
                    ),
            )
            .child(
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
            )
    }
}

/// 后台解码：读文件 → `image` crate 解码 → **BGRA** 像素（GPUI 渲染格式；
/// `image` crate 支持 png/jpg/gif/bmp/webp/ico/tiff 等）。返回 BGRA 字节 +
/// 像素宽高（主线程再包 RenderImage）。
fn decode_image_at(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes).map_err(|e| format!("无法解码图片：{e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    // RGBA → BGRA：每像素交换 R/B（GPUI 的 RenderImage 字节序为 BGRA，PDF 模块亦如此；
    // 不交换会红蓝互换、颜色明显失真）。
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok((bgra, w, h))
}

/// 扫描当前图片所在文件夹的全部图片（扩展名过滤、文件名升序），返回
/// (清单, 当前图下标)。失败/空目录时退化为「仅当前图」单元素清单。
fn scan_folder(at: &Path) -> (Vec<PathBuf>, usize) {
    let Some(dir) = at.parent() else {
        return (vec![at.to_path_buf()], 0);
    };
    let mut list: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() && crate::image::is_image_extension(&p) {
                list.push(p);
            }
        }
    }
    if list.is_empty() {
        return (vec![at.to_path_buf()], 0);
    }
    list.sort();
    let idx = list.iter().position(|p| p == at).unwrap_or(0);
    (list, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每测试独立的临时目录（按进程 id + 标签隔离，避免并行测试互相踩）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-img-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一张真实 PNG（`image` 默认特性可编码；JPEG 不支持 RGBA 编码，故其它
    /// 扩展名用例直接用占位字节——扫描只看扩展名，不看内容）。
    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200_u8, 30, 10, 255]));
        img.save(path).expect("本测试自产 PNG 应能写出");
    }

    #[test]
    fn decode_png_roundtrips_dimensions_and_bgra_order() {
        let dir = tmp_dir("decode");
        let p = dir.join("red.png");
        write_png(&p, 40, 20);
        let (bgra, w, h) = decode_image_at(&p).expect("自产 PNG 应能解码");
        assert_eq!((w, h), (40, 20));
        assert_eq!(bgra.len(), (40 * 20 * 4) as usize);
        // 写入色为 RGBA(200,30,10,255)；解码后必须输出 **BGRA**（GPUI 渲染格式）＝
        // [蓝10, 绿30, 红200, 255]。若这里仍是 RGBA 序，说明红蓝互换的修复回退。
        let c = [bgra[0], bgra[1], bgra[2], bgra[3]];
        assert_eq!(c, [10, 30, 200, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_missing_file_reports_error() {
        let p = tmp_dir("missing").join("没有的文件.png");
        assert!(decode_image_at(&p).is_err());
    }

    #[test]
    fn scan_folder_sorts_images_ignores_others_and_locates_current() {
        let dir = tmp_dir("scan");
        write_png(&dir.join("b.png"), 8, 8);
        write_png(&dir.join("a.png"), 8, 8);
        // JPEG 不支持 RGBA 编码，占位字节足够（扫描只看扩展名）。
        std::fs::write(dir.join("c.jpeg"), "fake jpeg bytes").unwrap();
        std::fs::write(dir.join("note.md"), "not an image").unwrap();
        std::fs::write(dir.join("logo.WEBP"), "fake webp bytes").unwrap();
        let current = dir.join("b.png");
        let (list, idx) = scan_folder(&current);
        let names: Vec<String> = list
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // md 排除；WEbP 大小写不敏感识别；清单按文件名升序。
        assert_eq!(names, vec!["a.png", "b.png", "c.jpeg", "logo.WEBP"]);
        assert_eq!(idx, 1);
        assert_eq!(list[idx], current);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_folder_empty_dir_falls_back_to_single_item() {
        let p = tmp_dir("orf").join("orphan.png");
        let (list, idx) = scan_folder(&p);
        assert_eq!(list.len(), 1);
        assert_eq!(idx, 0);
        assert_eq!(list[0], p);
    }
}
