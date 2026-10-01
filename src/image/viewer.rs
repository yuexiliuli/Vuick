//! 图片查看器视图：自包含的图片浏览组件（`ImageViewer`）。
//!
//! 设计（对齐 [`crate::pdf::PdfViewer`] 的「自包含阅读器」姿势）：
//! - **打开即显**：`image` crate 在后台线程解码（不阻塞 UI），主线程把 **BGRA** 像素
//!   （GPUI 渲染格式）包成 [`gpui::RenderImage`] 上屏；
//! - **单滚轮切换**：打开时扫描**同文件夹**的全部图片（扩展名过滤、文件名升序），
//!   此后单滚轮在清单内循环切换（向下滚 → 下一张，向上滚 → 上一张）；
//! - **Ctrl+滚轮缩放**：缩放倍率作用于「适应视口（Contain）」的基准尺寸，图片始终
//!   水平居中；缩放锚点为图像中心；
//!
//! # 流畅切换的三级流水线（P1/P2，参数见 [`crate::config::image`]）
//! - **方向感知的**双向**级联预取**（滑动窗口）：沿最近滚动方向铺随后 [`PREFETCH_FORWARD`] 张，
//!   余量时再保留 [`PREFETCH_BACKWARD`] 张刚看过的回看侧（回看不因 LRU 淘汰变冷）；窗口按到
//!   当前图距离封口，绝不超解挤掉近前图 + 窗口后预读字节——打开/停留时后台把「剩余图片」
//!   解码缓存好，快速连续滚轮（前进/回看）基本全命中缓存；
//! - **多核并行解码池**：同时最多 [`MAX_DECODE`] 张在解（当前目标 + 预取并发，预取封顶
//!   `MAX_DECODE-1` 预留 1 槽给冷目标秒占）；并发满时只记忆「最新显示目标」
//!   （[`Self::pending_target`]），空位一放立刻追最新——快速连滚既不吃满 4 核内部竞争，
//!   又不会每一滚都从头解码；
//! - **磁盘→RAM 字节预读**：文件**压缩字节** `fs::read` 进 LRU 字节缓存，与解码分离——
//!   解码从内存字节开始，不再等磁盘 IO；重解码/回看不重读盘；
//! - **解码结果 LRU 缓存**（张数 + 字节双上限）：命中即 O(1) 瞬时上屏。
//!
//! 落盘守卫：一次解码结果只在「仍是当前显示目标（`self.path == decode_path`）」时才
//! 上屏，乱序/过期的中间结果只进缓存不抢显示——彻底杜绝旧图覆盖新图。
//!
//! # 滚轮手势（P1）
//! - 行滚轮（鼠标）：1 事件 = 1 张；
//! - 像素滚轮（触控板）：累计像素超过 [`WHEEL_PIXEL_STEP`] 才切一张，把洪水事件合并
//!   成离散步进，不一次飞过多张。
//!
//! # 通知宿主
//! `notify()` + `refresh_windows()` 双保险（实体未注册 window invalidator 时仅 notify
//! 是空操作），宿主 `EditorApp` 据此同步标签标题与状态栏。

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

use crate::config::image::{
    BYTE_CACHE_BYTES, CACHE_MAX_BYTES, CACHE_MAX_ITEMS, FAST_SCROLL_STEP_MS, FAST_SCROLL_THRESHOLD,
    MAX_DECODE, PREFETCH_BACKWARD, PREFETCH_FORWARD, PREFETCH_FORWARD_FAST,
};

/// 一张已解码可立即上屏的图片（LRU 缓存条目）。
#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    render: Arc<RenderImage>,
    intrinsic: (f32, f32),
    bytes: usize,
}

/// 原始文件压缩字节（磁盘→RAM 预读层，LRU 字节预算缓存；解码从这里取字节免等磁盘）。
#[derive(Clone)]
struct ByteEntry {
    path: PathBuf,
    bytes: Arc<Vec<u8>>,
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
    /// 原始文件字节 LRU 缓存（磁盘预读层；解码优先从这里取字节）。
    byte_cache: VecDeque<ByteEntry>,
    /// 字节缓存总字节（预算 [`BYTE_CACHE_BYTES`]）。
    byte_cache_bytes: usize,
    /// 已在后台解码中的路径去重（当前目标 + 预取共用，防同一路径重复解码）。
    in_flight: HashSet<PathBuf>,
    /// 字节预读在途去重（与解码池分离：IO 便宜，不占 [`MAX_DECODE`] 解码槽）。
    byte_flight: HashSet<PathBuf>,
    /// 解码池并发满时记忆的「最新显示目标」：空位一放立即派发（追最新，跳过中间）。
    pending_target: Option<PathBuf>,
    /// 像素滚轮累计滚动（px）：超过 [`crate::config::image::WHEEL_PIXEL_STEP`] 才切一张。
    scroll_accum: f32,
    /// 级联预取方向（1 = 前进 / -1 = 回退；由最近一次切图方向决定）。
    prefetch_dir: i32,
    /// 级联预取「逻辑游标」：无界 i64，对 `folder.len()` 取模定位；每次派发后前进一格。
    prefetch_pos: i64,
    /// 当前级联的锚点（= 最近安装图片的下标）：游标越过锚点一整圈（即已覆盖全清单）后
    /// 停下歇着，避免空闲时把整个文件夹循环解个没完（LRU 也会淘汰，白费 CPU）。
    prefetch_origin: i64,
    /// 当前「预取」解码在途数（区别于显示目标解码）：用于预留解码槽——预取封顶
    /// [`MAX_DECODE`]-1，保证冷目标能立刻占到一个槽、不用排在预取队尾等。
    prefetch_inflight: usize,
    /// 快速连续滚动标志（B4）：连续滚准确认后临时放宽前向窗口、舍弃反向窗口。
    scroll_fast: bool,
    /// 最近一次滚动的方向（±1），用于判断「连续同向」。
    last_scroll_dir: i32,
    /// 连续同向快速滚动计数（进入/退出用 [`FAST_SCROLL_THRESHOLD`]，带迟滞）。
    fast_count: u32,
    /// 上一步滚动的时间戳（用于计算步间隔）。
    last_scroll_at: Option<std::time::Instant>,
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
            byte_cache: VecDeque::new(),
            byte_cache_bytes: 0,
            in_flight: HashSet::new(),
            byte_flight: HashSet::new(),
            pending_target: None,
            scroll_accum: 0.0,
            prefetch_dir: 1,
            prefetch_pos: 1,
            prefetch_origin: 0,
            prefetch_inflight: 0,
            scroll_fast: false,
            last_scroll_dir: 1,
            fast_count: 0,
            last_scroll_at: None,
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
                // 注意：**不在打开时抢跑预取**——首图先独占解码槽、尽快出现；首图安装
                // （install_current）后才启动级联预取。否则大图（如 20MP 照片）首图解码会
                // 和 3 个预取抢 4 核，打开反而更慢、且瞬时堆大量内存。
                this.request_display(cx);
                // 这里不再立即 pump_prefetch：等 install_current 触发。
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
        self.byte_cache.clear();
        self.byte_cache_bytes = 0;
        self.in_flight.clear();
        self.byte_flight.clear();
        self.pending_target = None;
        self.prefetch_inflight = 0;
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
            match event.delta {
                // 行滚轮（鼠标）：1 事件 = 1 张。
                ScrollDelta::Lines(_) => {
                    self.switch(if delta_y < 0.0 { 1 } else { -1 }, cx);
                }
                // 像素滚轮（触控板）：累计像素，超过一步门限才切（合并洪水事件）。
                ScrollDelta::Pixels(_) => {
                    self.scroll_accum += delta_y;
                    let step = crate::config::image::WHEEL_PIXEL_STEP;
                    while self.scroll_accum >= step {
                        self.switch(1, cx);
                        self.scroll_accum -= step;
                    }
                    while self.scroll_accum <= -step {
                        self.switch(-1, cx);
                        self.scroll_accum += step;
                    }
                }
            }
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
    /// 池解码追最新」。方向变化时重置级联预取锚点；同时做 B4「快速连续滚动」判定（连续
    /// 同向且步间隔 ≤ [`FAST_SCROLL_STEP_MS`] 累计到阈值 → 临时放宽前向窗口）。
    fn switch(&mut self, dir: i32, cx: &mut Context<Self>) {
        let n = self.folder.len();
        if n <= 1 || dir == 0 {
            return;
        }
        let next = ((self.current as i64 + dir as i64).rem_euclid(n as i64)) as usize;
        if next == self.current {
            return;
        }
        if dir != self.prefetch_dir {
            self.prefetch_dir = dir;
        }
        // B4：连续同向 + 快速步间隔 → 快滚计数；慢速/换向衰减（迟滞进出，避免抖）
        let now = std::time::Instant::now();
        let rapid = self.last_scroll_dir == dir
            && self
                .last_scroll_at
                .is_some_and(|t| now.duration_since(t).as_millis() as u64 <= FAST_SCROLL_STEP_MS);
        self.last_scroll_dir = dir;
        self.last_scroll_at = Some(now);
        if rapid {
            self.fast_count = self.fast_count.saturating_add(1);
        } else {
            // 慢速/换向：计数折半（自然衰减）；归零才退出快速模式（迟滞）
            self.fast_count /= 2;
        }
        // 进入阈值 FAST_SCROLL_THRESHOLD；进入后只要计数 > 0 保持（迟滞，避免抖进抖出）
        self.scroll_fast =
            self.fast_count >= FAST_SCROLL_THRESHOLD || (self.scroll_fast && self.fast_count > 0);
        self.current = next;
        self.path = self.folder[next].clone();
        self.request_display(cx);
    }

    /// 显示入口（首图 / 切换共用）：`path` 已指向目标。缓存命中 → 瞬时落盘并启动级联
    /// 预取；未中 → 置 loading 并进入「并行解码池」（去重 / 追最新）。
    fn request_display(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        if let Some(entry) = self.cache_get(&path) {
            self.install_current(entry, &path, cx);
            return;
        }
        self.loading = true;
        self.load_error = None;
        self.queue_decode(path, true, cx);
    }

    /// 解码池入队：已在解 → 等其落盘追最新；已缓存 → 直接落盘；并发满（[`MAX_DECODE`]）
    /// → 仅**显示目标**（`is_target`）记入 [`pending_target`] 等空位（预取在满时直接放弃）；
    /// 否则派发并行解码。`in_flight` 去重。
    fn queue_decode(&mut self, path: PathBuf, is_target: bool, cx: &mut Context<Self>) {
        if self.in_flight.contains(&path) {
            return;
        }
        if let Some(entry) = self.cache_get(&path) {
            if is_target && path == self.path {
                self.install_current(entry, &path, cx);
            }
            return;
        }
        if self.in_flight.len() >= MAX_DECODE {
            if is_target {
                self.pending_target = Some(path);
            }
            return;
        }
        self.spawn_decode(path, !is_target, cx);
    }

    /// 派发一次并行解码（解码池槽位之一）：优先用字节缓存里的内存字节，否则后台读盘并把
    /// 字节回填缓存（重解码不重读盘）。结果只进缓存；仅当「仍是当前显示目标」才上屏。
    fn spawn_decode(&mut self, path: PathBuf, is_prefetch: bool, cx: &mut Context<Self>) {
        self.in_flight.insert(path.clone());
        if is_prefetch {
            self.prefetch_inflight += 1;
        }
        let bytes = self.byte_cache_get(&path);
        let decode_path = path;
        let read_path = decode_path.clone();
        cx.spawn(async move |weak, cx| {
            // 后台：取字节（缓存命中直接用；未中读盘）→ 解码 → 返回 (原始字节, 解码结果)。
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let raw: Arc<Vec<u8>> = match bytes {
                        Some(b) => b,
                        None => Arc::new(std::fs::read(&read_path).map_err(|e| e.to_string())?),
                    };
                    let decoded = decode_image_bytes(&raw);
                    Ok::<_, String>((raw, decoded))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.in_flight.remove(&decode_path);
                if is_prefetch {
                    this.prefetch_inflight -= 1;
                }
                match outcome {
                    Ok((raw, decoded)) => {
                        // 回填字节缓存（跳过再回看不重读盘）
                        this.byte_cache_put(
                            decode_path.clone(),
                            ByteEntry {
                                path: decode_path.clone(),
                                bytes: raw,
                            },
                        );
                        match decoded {
                            Ok((bgra, w, h)) => {
                                if let Some(buf) = RgbaImage::from_raw(w, h, bgra) {
                                    let entry = CacheEntry {
                                        path: decode_path.clone(),
                                        render: Arc::new(RenderImage::new(vec![Frame::new(buf)])),
                                        intrinsic: (w as f32, h as f32),
                                        bytes: w as usize * h as usize * 4,
                                    };
                                    this.cache_put(decode_path.clone(), entry.clone());
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
                // 并发槽空出 → 追最新未派发目标，并顺势把预取窗口再往前推一格
                this.drain_pending(cx);
                this.pump_prefetch(cx);
            });
        })
        .detach();
    }

    /// 并发槽空出后的兜底：把「最新显示目标」重新入队（已在解 / 已缓存的情况下
    /// `queue_decode` 自行处理）。
    fn drain_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.pending_target.take() {
            self.queue_decode(p, true, cx);
        }
    }

    /// 方向感知的**双向级联预取**（滑动窗口）：
    /// 1. **主方向**（最近的滚动方向）：随后 `PREFETCH_FORWARD`（快速滚动放宽到
    ///    [`PREFETCH_FORWARD_FAST`]）张逐个排队解码，窗口按到当前图的距离封口——绝不超前
    ///    解到窗口外（否则会把用户正要滚到的图从 LRU 挤掉）；
    /// 2. **反向**（刚看过的往回侧）：非快速滚动时保留 [`PREFETCH_BACKWARD`] 张，回看上一张/
    ///    回头翻不因 LRU 淘汰变冷；快速滚动时让位给前方深预取；
    /// 3. 解码槽全满时给窗口外最近几张预读字节（IO 不占解码槽）。
    /// 4. 预取统一封顶 `MAX_DECODE - 1` 个在途解码（预留 1 槽给「冷目标」秒占）；显示目标在等槽位优先、
    ///    窗口铺满、或游标越过锚点一整圈（全清单已覆盖）时自停。
    fn pump_prefetch(&mut self, cx: &mut Context<Self>) {
        if self.pending_target.is_some() {
            return;
        }
        let n = self.folder.len() as i64;
        if n <= 1 {
            return;
        }
        // B4：快速滚动 → 放宽前向窗口
        let forward_win = if self.scroll_fast {
            PREFETCH_FORWARD_FAST
        } else {
            PREFETCH_FORWARD
        } as i64;
        // 1) 主方向窗口：dist 1..=forward_win（预取封顶 MAX_DECODE-1，预留冷目标槽）
        while (self.prefetch_pos - self.prefetch_origin).abs() < n
            && self.prefetch_inflight < MAX_DECODE - 1
            && (self.prefetch_pos - self.prefetch_origin).abs() <= forward_win
        {
            let ni = self.prefetch_pos.rem_euclid(n) as usize;
            let path = self.folder[ni].clone();
            self.prefetch_pos += self.prefetch_dir as i64;
            if self.cache_contains(&path) || self.in_flight.contains(&path) {
                continue;
            }
            self.queue_decode(path, false, cx);
            if self.pending_target.is_some() {
                return;
            }
        }
        // 2) 反向窗口：非快速滚动时，当前图往回 PREFETCH_BACKWARD 张（低优先，仅余量时补）
        if !self.scroll_fast {
            let base = self.current as i64;
            for k in 1..=PREFETCH_BACKWARD as i64 {
                if self.prefetch_inflight + 1 >= MAX_DECODE || self.cache.len() >= CACHE_MAX_ITEMS
                {
                    break;
                }
                let ni = (base - k).rem_euclid(n) as usize;
                let path = self.folder[ni].clone();
                if self.cache_contains(&path) || self.in_flight.contains(&path) {
                    continue;
                }
                self.queue_decode(path, false, cx);
                if self.pending_target.is_some() {
                    return;
                }
            }
        }
        // 3) 解码槽全满：给窗口后最近几张预读字节（IO 不占解码槽；等空位时不再等磁盘）
        if self.prefetch_inflight + 1 >= MAX_DECODE {
            for _ in 0..MAX_DECODE {
                if (self.prefetch_pos - self.prefetch_origin).abs() >= n {
                    break;
                }
                let ni = self.prefetch_pos.rem_euclid(n) as usize;
                let path = self.folder[ni].clone();
                self.prefetch_pos += self.prefetch_dir as i64;
                self.preload_bytes(path, cx);
            }
        }
    }

    /// 磁盘→RAM 字节预读：把相邻文件压缩字节读进 LRU 缓存（IO 与解码分离，便宜便宜）。
    /// 去重：已在字节缓存 / 字节预读在途 / 正在解码（解码自身会读盘回填）则跳过。
    fn preload_bytes(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.byte_cache_contains(&path)
            || self.byte_flight.contains(&path)
            || self.in_flight.contains(&path)
        {
            return;
        }
        self.byte_flight.insert(path.clone());
        let p = path;
        let p_inner = p.clone();
        cx.spawn(async move |weak, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&p_inner).ok().map(Arc::new) })
                .await;
            let _ = weak.update(cx, |this, _cx| {
                this.byte_flight.remove(&p);
                if let Some(b) = read
                    && !this.byte_cache_contains(&p)
                {
                    this.byte_cache_put(
                        p.clone(),
                        ByteEntry {
                            path: p,
                            bytes: b,
                        },
                    );
                }
            });
        })
        .detach();
    }

    /// 把缓存条目落盘为当前显示 + 通知宿主 + 重锚级联预取（窗口沿方向滑动更新）。
    fn install_current(&mut self, entry: CacheEntry, path: &Path, cx: &mut Context<Self>) {
        self.path = path.to_path_buf();
        self.image = Some(entry.render);
        self.intrinsic = Some(entry.intrinsic);
        self.loading = false;
        self.load_error = None;
        cx.notify();
        cx.refresh_windows();
        // 锚点移到当前位置：前向/反向窗口随位置滑动，继续沿方向补满
        self.prefetch_origin = self.current as i64;
        self.prefetch_pos = self.current as i64 + self.prefetch_dir as i64;
        self.pump_prefetch(cx);
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

    /// 字节缓存查询并移到队尾（MRU），返回 Arc 克隆（保留在缓存里供解码/重解码复用）。
    fn byte_cache_get(&mut self, path: &Path) -> Option<Arc<Vec<u8>>> {
        if let Some(pos) = self.byte_cache.iter().position(|e| e.path == path) {
            let entry = self.byte_cache.remove(pos).unwrap();
            let bytes = entry.bytes.clone();
            self.byte_cache.push_back(entry);
            Some(bytes)
        } else {
            None
        }
    }

    /// 字节缓存是否已含某路径（只读，不改变 LRU 顺序）。
    fn byte_cache_contains(&self, path: &Path) -> bool {
        self.byte_cache.iter().any(|e| e.path == path)
    }

    /// 写入字节缓存（已存在则移到队尾），超出字节预算 [`BYTE_CACHE_BYTES`] 按 LRU 淘汰。
    fn byte_cache_put(&mut self, path: PathBuf, entry: ByteEntry) {
        if let Some(pos) = self.byte_cache.iter().position(|e| e.path == path) {
            self.byte_cache_bytes -= self.byte_cache.remove(pos).unwrap().bytes.len();
        }
        self.byte_cache_bytes += entry.bytes.len();
        self.byte_cache.push_back(entry);
        while self.byte_cache_bytes > BYTE_CACHE_BYTES {
            let Some(old) = self.byte_cache.pop_front() else {
                break;
            };
            self.byte_cache_bytes -= old.bytes.len();
        }
    }

    /// C6：取滚动方向最近一张「已解码在缓存、但非当前显示」的图，包成**垫底透明 img**
    /// （同尺寸、opacity 0、不拦截输入）。让 gpui 在上屏前就完成该纹理的 GPU 上传，切到
    /// 的那一帧不再肩负首次上传（真实 GPU 生效；headless 测试无真实纹理，仅保证构造正确）。
    fn peek_upgrade_element(&self) -> Option<gpui::AnyElement> {
        let n = self.folder.len() as i64;
        if n <= 1 {
            return None;
        }
        let ni = (self.current as i64 + self.prefetch_dir as i64).rem_euclid(n) as usize;
        let target = self.folder.get(ni)?;
        if target == &self.path {
            return None;
        }
        let entry = self.cache.iter().find(|e| e.path == *target)?;
        let render = entry.render.clone();
        Some(
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
        )
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

        // C6：把方向最近的已缓存图以透明 img 垫底，纹理预先上传（详见 peek_upgrade_element）
        let preheat = self.peek_upgrade_element();

        let mut root = div()
            .id("image-viewer")
            .size_full()
            .relative()
            .bg(canvas);
        if let Some(peek) = preheat {
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
            );
        root
    }
}

/// 后台解码（输入内存字节）：`image` crate 解码 → **BGRA** 像素（GPUI 渲染格式；
/// `image` crate 支持 png/jpg/gif/bmp/webp/ico/tiff 等）。返回 BGRA 字节 + 像素宽高。
/// 与磁盘 IO 分离（[`decode_image_at`] 负责读盘），解码流水线可从字节缓存直接调用。
fn decode_image_bytes(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("无法解码图片：{e}"))?;
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

/// 读盘 + 解码（[`decode_image_bytes`]）。生产流水线走「字节缓存 / 后台 read + 解码」，
/// 此包装仅测试（含 bench）与回退路径使用。
#[cfg(test)]
fn decode_image_at(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_image_bytes(&bytes)
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
    use gpui::AppContext as _;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;

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

    /// 实测「切换图片」耗时：对指定文件夹内每张图跑一次 `decode_image_at`（=冷缓存切换
    /// 时查看器实际等待的解码链路：fs::read + image 解码 + BGRA 交换）。缓存命中时切换
    /// 是 O(1) 瞬时（接近 0，主要剩首次 GPU 上传）。目录可用 `IMG_BENCH_DIR` 覆盖，
    /// 默认测 `C:\Users\20294\Pictures\ave mujica`。用 `--ignored --nocapture` 运行。
    #[test]
    #[ignore]
    fn bench_decode_folder() {
        let dir = std::env::var("IMG_BENCH_DIR").unwrap_or_else(|_| {
            "C:\\Users\\20294\\Pictures\\ave mujica".to_string()
        });
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("读取目录失败 {dir}: {e}"))
            .flatten()
            .map(|e| e.path())
            .filter(|p| crate::image::is_image_extension(p))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "目录内无图片可测：{dir}");
        // 可选上限（`IMG_BENCH_LIMIT`），测大文件夹时只跑前几张即可看数量级
        if let Ok(lim) = std::env::var("IMG_BENCH_LIMIT")
            && let Ok(n) = lim.parse::<usize>()
        {
            files.truncate(n);
        }
        eprintln!(
            "\n[IMG-BENCH] {} 张 · 每张冷解码耗时（fs::read + image 解码 + RGBA→BGRA）：",
            files.len()
        );
        let mut samples: Vec<(f64, u64)> = Vec::new();
        let mut total_bytes = 0u64;
        for p in &files {
            let t = std::time::Instant::now();
            let res = decode_image_at(p);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let meta = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            total_bytes += meta;
            match res {
                Ok((_bgra, w, h)) => {
                    eprintln!(
                        "  {:7.1}ms  {:>9} px  {:6.1} KB  {}",
                        ms,
                        w as usize * h as usize,
                        meta as f64 / 1024.0,
                        p.file_name().unwrap().to_string_lossy()
                    );
                    samples.push((ms, meta));
                }
                Err(e) => eprintln!("  ERR {}: {e}", p.display()),
            }
        }
        if !samples.is_empty() {
            let sum: f64 = samples.iter().map(|s| s.0).sum();
            let min = samples.iter().map(|s| s.0).fold(f64::MAX, f64::min);
            let max = samples.iter().map(|s| s.0).fold(f64::MIN, f64::max);
            eprintln!(
                "  --- 冷解码：均值 {:.1}ms / min {:.1}ms / max {:.1}ms / 共 {} 张 / 总 {:.1} MB",
                sum / samples.len() as f64,
                min,
                max,
                samples.len(),
                total_bytes as f64 / (1024.0 * 1024.0)
            );
            eprintln!(
                "  --- 切换体感：缓存命中 ≈ 0ms（瞬时 + 首次 GPU 上传）；缓存未中 ≈ 该图冷解码 ≈ {:.1}ms",
                sum / samples.len() as f64
            );
        }
    }

    /// 写一张有内容（渐变）的 PNG——纯色会压缩成接近 0 字节、解码快，测不出差距。
    fn write_pattern_png(path: &Path, w: u32, h: u32) {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            let b = ((x + y) * 60 / (w + h)) as u8;
            *px = image::Rgba([r, g, b, 255]);
        }
        img.save(path).expect("写出渐变 PNG 应成功");
    }

    /// 把测试执行器里的异步任务（含后台解码/预读链）全部排空。
    fn flush(cx: &mut TestAppContext) {
        for _ in 0..8 {
            cx.run_until_parked();
        }
    }

    /// 等到「当前显示目标已就绪」（loading==false 且已非首帧）：缓存命中时首次刻就绪
    /// （≈0）；缓存未中时轮询执行器直到后台解码落盘。带安全上限防死循环。
    fn wait_until_ready(cx: &mut TestAppContext, viewer: &gpui::Entity<ImageViewer>) {
        let mut guard = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                !v.loading && v.image.is_some()
            });
            if ready {
                return;
            }
            cx.run_until_parked();
            guard += 1;
            assert!(guard < 2000, "等待目标图片就绪超时");
        }
    }

    /// P3 端到端感知延迟（真实流水线，`--ignored --nocapture` 运行）：
    /// - 打开首图 = 冷路径（扫描 + 首图解码 + 级联预取把后续图并行解码进缓存）；
    /// - 连续向前切换 = 暖路径（预取/缓存命中，切图瞬间同步落盘，近 0；
    ///   `n=40` 远超缓存窗口 6，验证滑动/淘汰下连续滚动依旧全命中）；
    /// - 大步跳变 = 跳离预取窗口（LRU 淘汰后回看：字节缓存 + 重解码路径）。
    #[gpui::test]
    #[ignore]
    async fn bench_switch_pipeline(cx: &mut TestAppContext) {
        use std::time::Instant;
        let dir = tmp_dir("pipeline");
        let n = 40u32;
        for i in 0..n {
            write_pattern_png(&dir.join(format!("{:02}.png", i)), 1200, 900);
        }
        let first = dir.join("00.png");
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(first, cx)));

        // 打开 → **首图就绪**（不含预取耗尽）＝ 用户感知的「打开出图」耗时
        let t0 = Instant::now();
        let mut open_guard = 0;
        loop {
            cx.run_until_parked();
            if cx.update(|app| viewer.read(app).is_loaded()) {
                break;
            }
            open_guard += 1;
            assert!(open_guard < 2000, "等待首图就绪超时");
        }
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // 再排空预取链（bench 全量推进，供后续滚动全命中缓存）
        flush(cx);
        let (cache_len, folder_len) = cx.update(|app| {
            let v = viewer.read(app);
            (v.cache.len(), v.folder.len())
        });
        assert!(
            cache_len >= 3,
            "打开后应已预取至少 3 张进缓存（级联预取没生效？）：cache={cache_len}/{folder_len}"
        );

        // 连续向前切 n-1 次：测「switch → 当前图就绪」（暖，应 ≈0，预取已命中缓存）
        let mut warm: Vec<f64> = Vec::new();
        for _ in 0..(n - 1) {
            let t = Instant::now();
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
            wait_until_ready(cx, &viewer);
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
            flush(cx); // 推进下一张的级联预取链（不计入 timing）
        }
        let avg_warm = warm.iter().sum::<f64>() / warm.len() as f64;

        // 反向 39 次（回看上一张）：双向窗口应让回看也全命中缓存（对应用户「向前滚动卡」）
        let mut back: Vec<f64> = Vec::new();
        for _ in 0..(n - 1) {
            let t = Instant::now();
            cx.update(|app| viewer.update(app, |v, cx| v.switch(-1, cx)));
            wait_until_ready(cx, &viewer);
            back.push(t.elapsed().as_secs_f64() * 1000.0);
            flush(cx);
        }
        let avg_back = back.iter().sum::<f64>() / back.len() as f64;
        let min_back = back.iter().cloned().fold(f64::MAX, f64::min);

        // 大步跳变（跳离窗口到 LRU 已挤掉的一张）：字节缓存 + 重解码路径（真实等待）
        let t = Instant::now();
        cx.update(|app| viewer.update(app, |v, cx| v.switch(25, cx)));
        wait_until_ready(cx, &viewer);
        let jump_ms = t.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "\n[IMG-PIPELINE] 打开→首图 {:8.2}ms · 前进切换(暖) 平均 {:8.3}ms · 回看切换(暖) 平均 {:8.3}ms / min {:8.3}ms · 大步跳变(冷) {:8.2}ms \
             · 打开后缓存 {cache_len}/{folder_len} ({} 张 1200×900, 前向 {} 反向 {})",
            open_ms,
            avg_warm,
            avg_back,
            min_back,
            jump_ms,
            n,
            crate::config::image::PREFETCH_FORWARD,
            crate::config::image::PREFETCH_BACKWARD
        );
        // 前进与回看都应远快于冷打开（预取/缓存生效）
        assert!(
            avg_warm < open_ms / 8.0,
            "前进切换应远低于冷打开：{avg_warm}ms vs {open_ms}ms"
        );
        assert!(
            avg_back < open_ms / 8.0,
            "回看切换应远低于冷打开：{avg_back}ms vs {open_ms}ms"
        );

        // B4：快速连续滚动应进入快速模式、前向窗口放宽到 PREFETCH_FORWARD_FAST
        for _ in 0..(PREFETCH_FORWARD_FAST + 8) {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        let fast_now = cx.update(|app| viewer.read(app).scroll_fast);
        flush(cx);
        let reach = cx.update(|app| {
            let v = viewer.read(app);
            (v.prefetch_pos - v.prefetch_origin).abs()
        });
        assert!(fast_now, "快速连续滚动应进入快速模式");
        assert!(
            reach >= PREFETCH_FORWARD_FAST as i64,
            "快速模式前向窗口应放宽到 {PREFETCH_FORWARD_FAST}，实为 {reach}"
        );
        eprintln!("[IMG-PIPELINE] B4 快速模式：进入 ✓ · 前向覆盖 {reach}（目标 ≥{PREFETCH_FORWARD_FAST}）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D8：量到「首帧上屏」——在**真实窗口**里度量 `switch → 图就绪 → 首帧绘制完成`
    /// 的耗时（经过窗口 render/布局/绘制路径；headless 无真实 GPU 上传，但比「只到解码
    /// 就绪」多覆盖「上屏」这一段调度）。注：`add_window_view` 会先绘一帧（冷解码在其内
    /// 完成），故只统计**切换**→首帧，不统计打开。用 `--ignored --nocapture` 运行。
    #[gpui::test]
    #[ignore]
    async fn bench_switch_to_paint(cx: &mut TestAppContext) {
        use std::time::Instant;
        let dir = tmp_dir("paint");
        let n = 8u32;
        for i in 0..n {
            write_pattern_png(&dir.join(format!("{:02}.png", i)), 1200, 900);
        }
        let first = dir.join("00.png");
        let (viewer, wcx) = cx.add_window_view(|_window, cx| ImageViewer::new(first, cx));
        // 等 opens 置备完成（冷解码在 add_window_view 的预绘制帧内已完成）
        wcx.run_until_parked();

        // 画一帧（标记脏 + 排空执行器走到绘制）
        fn paint(wcx: &mut VisualTestContext) {
            let _ = wcx.refresh();
            wcx.run_until_parked();
        }

        // 暖切换 ×6：测「switch → 就绪 → 首帧绘制完成」
        let mut paints: Vec<f64> = Vec::new();
        for _ in 0..6 {
            let t = Instant::now();
            wcx.update(|_, cx| viewer.update(cx, |v, cx| v.switch(1, cx)));
            let mut g = 0;
            loop {
                wcx.run_until_parked();
                if wcx.update(|_, cx| viewer.read(cx).is_loaded()) {
                    break;
                }
                g += 1;
                assert!(g < 2000, "等待切换图就绪超时");
            }
            paint(wcx);
            paints.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let avg_paint = paints.iter().sum::<f64>() / paints.len() as f64;
        let min_paint = paints.iter().cloned().fold(f64::MAX, f64::min);

        eprintln!(
            "\n[IMG-PAINT] 暖切换→首帧上屏 平均 {:8.3}ms / min {:8.3}ms ({} 张 1200×900, debug 无真实 GPU)",
            avg_paint, min_paint, n
        );
        assert!(
            avg_paint < 50.0,
            "暖切换→首帧应远小于 50ms：{avg_paint}ms"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
