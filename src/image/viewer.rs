//! 图片查看器视图：自包含的图片浏览组件（`ImageViewer`）。
//!
//! 设计（对齐 [`crate::pdf::PdfViewer`] 的「自包含阅读器」姿势）：
//! - **打开即显**：`image` crate 在后台线程解码（不阻塞 UI），主线程把 **BGRA** 像素
//!   （GPUI 渲染格式）包成 [`gpui::RenderImage`] 上屏；
//! - **单滚轮切换**：打开时扫描**同文件夹**的全部图片（扩展名过滤、文件名升序），
//!   此后单滚轮在清单内循环切换（向下滚 → 下一张，向上滚 → 上一张）；
//! - **Ctrl+滚轮缩放**：缩放倍率作用于「适应视口（Contain）」的基准尺寸，图片始终
//!   水平居中；缩放锚点为图像中心；
//! - **流畅切换（缓存 + 预取 + 单 worker 合并最新目标）**：
//!   - 解码结果进 **LRU 缓存**（张数 + 字节双上限），滚动即切时多数是 O(1) 缓存命中、
//!     瞬时上屏；
//!   - 当前图加载完立刻**后台预取相邻 ±1 张**（含循环回绕），正向/反向浏览几乎零等待；
//!   - **单解码 worker**只追「最新显示目标」：快速连滚时中间图只解最新一张、乱序的
//!     旧结果不落盘，且 `in_flight` 去重避免同一路径重复解码——不再有「每步必等解码 /
//!     重复解码白耗 CPU+IO」的卡顿；
//! - **通知宿主**：`notify()` + `refresh_windows()` 双保险（实体未注册 window
//!   invalidator 时仅 notify 是空操作），宿主 `EditorApp` 据此同步标签标题与状态栏。

use std::collections::{HashSet, VecDeque};
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
/// 解码缓存上限（张数）：超出按 LRU 淘汰（配合字节预算，见 [`CACHE_MAX_BYTES`]）。
const CACHE_MAX_ITEMS: usize = 4;
/// 解码缓存字节预算（RGBA 近似内存）：超出按 LRU 淘汰，避免大图缓存无界膨胀。
const CACHE_MAX_BYTES: usize = 96 * 1024 * 1024;

/// 一张已解码可立即上屏的图片（LRU 缓存条目）。
#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    render: Arc<RenderImage>,
    intrinsic: (f32, f32),
    bytes: usize,
}

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
    /// 解码结果 LRU 缓存（队首最旧 / 队尾最近用；命中与写入都移到队尾）。
    cache: VecDeque<CacheEntry>,
    /// 缓存总字节（RGBA 近似内存，淘汰预算 [`CACHE_MAX_BYTES`]）。
    cache_bytes: usize,
    /// 已在后台解码中的路径去重（当前目标 worker + 预取共用，防同一路径重复解码）。
    in_flight: HashSet<PathBuf>,
    /// 当前目标解码 worker 是否在途（串行：一次只解「最新显示目标」一张；其余快速
    /// 连滚的中间目标被合并，等本次落盘后再追最新）。
    decoding: bool,
}

impl ImageViewer {
    /// 创建查看器并异步加载 `path`（同文件夹扫描在后台；首图经缓存/worker 流程加载）。
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
            cache: VecDeque::new(),
            cache_bytes: 0,
            in_flight: HashSet::new(),
            decoding: false,
        };
        let scan_at = path.clone();
        cx.spawn(async move |weak, cx| {
            let (folder, current) = cx
                .background_executor()
                .spawn(async move { scan_folder(&scan_at) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.folder = folder;
                this.current = current;
                // 统一入口：缓存命中瞬时落盘 / 未中由 worker 解码并预取 ±1。
                this.request_display(cx);
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

    /// 释放已上屏与缓存的 GPU 纹理（关闭标签页时调用，与 PDF 阅读器同款）。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(img) = self.image.take() {
            cx.drop_image(img, Some(window));
        }
        for entry in std::mem::take(&mut self.cache).into_iter() {
            cx.drop_image(entry.render, Some(window));
        }
        self.cache_bytes = 0;
        self.in_flight.clear();
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

    /// 在同文件夹清单内切换（`dir` = ±1，取模循环）。切换本身 O(1)：目标立即写入
    /// `path`（标题/状态栏跟着走），显示由 `request_display` 决定「缓存命中秒切 / 未中
    /// worker 追最新」。
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
        self.path = self.folder[next].clone();
        self.request_display(cx);
    }

    /// 显示入口（首图 / 切换 / 预取完成共用）：`path` 已指向目标。缓存命中 → 瞬时
    /// 落盘并预取邻居；未中 → 置 loading 并交给单 worker 解码。
    fn request_display(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        if let Some(entry) = self.cache_get(&path) {
            self.install_current(entry, &path, cx);
            return;
        }
        self.loading = true;
        self.load_error = None;
        self.pump_decode(cx);
    }

    /// 单解码 worker：一次只解「最新显示目标」一张；快速连滚时中间目标被合并（落盘时
    /// 已非当前 → 只进缓存不显示），旧结果不会覆盖新图。`in_flight` 去重防重复解码。
    fn pump_decode(&mut self, cx: &mut Context<Self>) {
        if self.decoding {
            return; // 上一张在途：落盘后会自动续追最新目标
        }
        let path = self.path.clone();
        // 可能已被预取填进缓存（等它到达期间切到该图）→ 直接秒切
        if let Some(entry) = self.cache_get(&path) {
            self.install_current(entry, &path, cx);
            return;
        }
        if self.in_flight.contains(&path) {
            return; // 预取正在解该图：其完成回调会 `request_display` 落盘
        }
        self.decoding = true;
        self.in_flight.insert(path.clone());
        let decode_path = path;
        let decode_inner = decode_path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { decode_image_at(&decode_inner) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.in_flight.remove(&decode_path);
                this.decoding = false;
                match result {
                    Ok((bgra, w, h)) => {
                        if let Some(buf) = RgbaImage::from_raw(w, h, bgra) {
                            let entry = CacheEntry {
                                path: decode_path.clone(),
                                render: Arc::new(RenderImage::new(vec![Frame::new(buf)])),
                                intrinsic: (w as f32, h as f32),
                                bytes: w as usize * h as usize * 4,
                            };
                            this.cache_put(decode_path.clone(), entry.clone());
                            // 仍旧是「最新目标」才落盘显示（否则仅供缓存）
                            if this.path == decode_path {
                                this.install_current(entry, &decode_path, cx);
                            }
                        } else if this.path == decode_path {
                            this.load_error = Some("解码结果尺寸异常".into());
                            this.loading = false;
                            cx.notify();
                            cx.refresh_windows();
                        }
                    }
                    Err(e) => {
                        if this.path == decode_path {
                            this.load_error = Some(e);
                            this.loading = false;
                            cx.notify();
                            cx.refresh_windows();
                        }
                    }
                }
                // 解码期间目标又移动 → 续追最新（否则停在旧图）
                if this.path != decode_path {
                    this.pump_decode(cx);
                }
            });
        })
        .detach();
    }

    /// 后台预取相邻 ±1 张（含循环回绕）：只填缓存不抢显示；回来后若恰是当前显示目标
    /// 则立即经 `request_display` 秒切。去重：已在缓存 / 已在解码中则跳过。
    fn prefetch_neighbors(&mut self, cx: &mut Context<Self>) {
        let n = self.folder.len();
        if n <= 1 {
            return;
        }
        for off in [1i64, -1i64] {
            let ni = ((self.current as i64 + off).rem_euclid(n as i64)) as usize;
            let path = self.folder[ni].clone();
            if self.cache_contains(&path) || self.in_flight.contains(&path) {
                continue;
            }
            self.in_flight.insert(path.clone());
            let p = path;
            let p_inner = p.clone();
            cx.spawn(async move |weak, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { decode_image_at(&p_inner) })
                    .await;
                let _ = weak.update(cx, |this, cx| {
                    this.in_flight.remove(&p);
                    // 预取结果恰是当前显示目标（loading 中）→ 成功经缓存落盘；失败须
                    // 亮错误并结束 loading，否则会一直停在旧图/占位。
                    let is_current = this.loading && this.path == p;
                    match result {
                        Ok((bgra, w, h)) => {
                            if let Some(buf) = RgbaImage::from_raw(w, h, bgra) {
                                let entry = CacheEntry {
                                    path: p.clone(),
                                    render: Arc::new(RenderImage::new(vec![Frame::new(buf)])),
                                    intrinsic: (w as f32, h as f32),
                                    bytes: w as usize * h as usize * 4,
                                };
                                this.cache_put(p.clone(), entry);
                                if is_current {
                                    this.request_display(cx);
                                }
                            } else if is_current {
                                this.load_error = Some("解码结果尺寸异常".into());
                                this.loading = false;
                                cx.notify();
                                cx.refresh_windows();
                            }
                        }
                        Err(e) => {
                            if is_current {
                                this.load_error = Some(e);
                                this.loading = false;
                                cx.notify();
                                cx.refresh_windows();
                            }
                        }
                    }
                });
            })
            .detach();
        }
    }

    /// 把缓存条目落盘为当前显示 + 通知宿主 + 预取邻居。
    fn install_current(&mut self, entry: CacheEntry, path: &Path, cx: &mut Context<Self>) {
        self.path = path.to_path_buf();
        self.image = Some(entry.render);
        self.intrinsic = Some(entry.intrinsic);
        self.loading = false;
        self.load_error = None;
        cx.notify();
        cx.refresh_windows();
        self.prefetch_neighbors(cx);
    }

    /// 缓存查询并移到队尾（MRU）。命中返回克隆（Arc 浅拷贝），未中 `None`。
    fn cache_get(&mut self, path: &Path) -> Option<CacheEntry> {
        if let Some(pos) = self.cache.iter().position(|e| e.path == path) {
            let entry = self.cache.remove(pos).unwrap();
            self.cache.push_back(entry.clone());
            Some(entry)
        } else {
            None
        }
    }

    /// 缓存是否已含某路径（只读，不改变 LRU 顺序）。
    fn cache_contains(&self, path: &Path) -> bool {
        self.cache.iter().any(|e| e.path == path)
    }

    /// 写入缓存（已存在则移到队尾），超出张数/字节预算按 LRU 从队首淘汰。
    fn cache_put(&mut self, path: PathBuf, entry: CacheEntry) {
        if let Some(pos) = self.cache.iter().position(|e| e.path == path) {
            self.cache_bytes -= self.cache[pos].bytes;
            self.cache.remove(pos);
        }
        self.cache_bytes += entry.bytes;
        self.cache.push_back(entry);
        while self.cache.len() > CACHE_MAX_ITEMS || self.cache_bytes > CACHE_MAX_BYTES {
            let Some(old) = self.cache.pop_front() else {
                break;
            };
            self.cache_bytes -= old.bytes;
        }
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
