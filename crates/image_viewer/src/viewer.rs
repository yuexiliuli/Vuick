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
//! # 流畅切换的三级流水线（P1/P2，参数见 [`config::image`]）
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
//! # 大图打开/滚动（school 20MP 照片，CPU/内存双降 + 清晰）
//! - **按展示像素解码（decode to display size）**：JPEG 走 `libjpeg-turbo`（纯 Rust SIMD：
//!   AVX2/SSE2 IDCT 降档 + 直出 BGRA，无 `DynamicImage` 中间拷贝）按 [`Self::display_need`]
//!   档位解码。选档规则 = **解码分辨率 ≥ 显示需要的像素**（上限 1/8，需要涨则分母落，
//!   需要超过原图才全解）——GPU 只缩小不放大 → **显示不糊**；单帧内存 75MB→~20MB。
//! - **内存/CPU 有界**：解码并发照旧 [`MAX_DECODE`]-1 预取封顶，且 [`Self::pump_prefetch`]
//!   在解码缓存达到字节预算后**停止派发**（[`CACHE_MAX_BYTES`]）——不再把整个文件夹扫一遍
//!   白算 CPU（此前 106 张全解 ≈ 8GB 像素量磨 8 核）。缓存能装 ~9 张显示档 → 滚动基本全命中。
//! - **放大升档**：Ctrl+滚轮使 `display_need` 变大 → [`Self::escalate_display_denom`] 按需
//!   重解更细档（只升不降，`in_flight` 去重防抖）——1:1 看细节时自动回到全解清晰图。
//! - **P1b 即时首帧（EXIF 嵌入缩略图，零依赖）**：冷打开 JPEG 先解出 APP1-Exif IFD1 内嵌
//!   ~160px 缩略图立即上屏（几 ms），展示档真图解码完成后覆盖——首帧不再空等熵解码（school
//!   已实测确实带缩略图；占位不升档/不抢预取槽/不进 LRU）。
//!
//! 落盘守卫：一次解码结果只在「仍是当前显示目标（`self.path == decode_path`）」时才
//! 上屏，乱序/过期的中间结果只进缓存不抢显示——彻底杜绝旧图覆盖新图。
//!
//! # 滚轮手势（P1）
//! - 行滚轮（鼠标）：1 事件 = 1 张；
//! - 像素滚轮（触控板）：累计像素超过 [`WHEEL_PIXEL_STEP`] 才切一张，把洪水事件合并
//!   成离散步进，不一次飞过多张；
//! - **Ctrl+滚轮缩放 + 滚轮/拖拽浏览**：放大后滚轮**优先在图片内平移**（滚到边缘继续同向
//!   外滚才切下/上一张），左键拖拽也可平移（开手/抓手光标）；**切图时缩放复位为适应视口
//!   、平移归零**（每张从初始大小开始）。
//!
//! # 通知宿主
//! `notify()` + `refresh_windows()` 双保险（实体未注册 window invalidator 时仅 notify
//! 是空操作），宿主 `EditorApp` 据此同步标签标题与状态栏。

use std::collections::{HashSet, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    div, img, px, App, Context, CursorStyle, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement, Render,
    RenderImage, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString, StatefulInteractiveElement,
    Styled, StyledImage, Window,
};
use image::{AnimationDecoder, Frame, ImageDecoder, RgbaImage};
use libjpeg_turbo_rs::{Decoder as LtjDecoder, PixelFormat as LtjPixelFormat, ScalingFactor};

use config::image::{
    BYTE_CACHE_BYTES, CACHE_MAX_BYTES, CACHE_MAX_ITEMS, FAST_SCROLL_STEP_MS, FAST_SCROLL_THRESHOLD,
    MAX_DECODE, PREFETCH_BACKWARD, PREFETCH_FORWARD, PREFETCH_FORWARD_FAST, ZOOM_MAX, ZOOM_MIN,
    ZOOM_STEP,
};
/// 放大态滚轮平移每格的视口比例（Lines 滚轮：一下滚轮平移多少视口高；直接切图的
/// 未放大态此值无意义）。
const WHEEL_LINE_PAN_FRACTION: f32 = 0.15;
/// 像素滚轮：每累计一个 [`WHEEL_PIXEL_STEP`] 平移的像素 = 步长 × 本倍率（放大态平移步进）。
const WHEEL_PIXEL_PAN_MULT: f32 = 4.0;
/// 展示像素需要初始值：窗口/视口数据未知（首个 render 前）时的合理猜测 ≈1080p，
/// 保证首解就是「显示尺寸档」而非低清档；首个 render 立即用真实视口校正。
const DISPLAY_NEED_INIT: (f32, f32) = (1920.0, 1080.0);
/// GIF 动图最多保留的帧数：超出只取前 N 帧（动画仍循环），资源护栏防超大 GIF 用帧数
/// 把内存/上传预算打爆（超大文件另有 LRU 字节预算如实兜底）。
const MAX_GIF_FRAMES: usize = 256;

use ::viewer::auto_hide;
use ::viewer::folder::FolderNav;
use ::viewer::pan_zoom;

/// 一张已解码可立即上屏的图片（LRU 缓存条目）。
#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    render: Arc<RenderImage>,
    intrinsic: (f32, f32),
    bytes: usize,
    /// JPEG 实际解码降档分母（1/8/4/2/1；非 JPEG 恒 1）。用于「升档判定」：
    /// 展示分母 > 当前需要分母（= 解码比显示更粗）时按需重解更细档（放大时）。
    denom: u8,
    /// 动图（GIF）逐帧切片：每帧一个**独立单帧** `RenderImage` + 各自时长；`None` = 静态图。
    /// 采用「按帧切片」而非 gpui 多帧 `RenderImage`：gpui `img()` 元素的自动动画只在窗口
    /// 聚焦（`window.is_window_active()`）时推进，失焦即冻结——改为本查看器用后台时钟
    /// 逐帧推进 `self.image`（单帧 RenderImage），每次渲染直接画当前帧，与窗口焦点无关。
    gif: Option<(Vec<Arc<RenderImage>>, Vec<std::time::Duration>)>,
}

/// 原始文件压缩字节（磁盘→RAM 预读层，LRU 字节预算缓存；解码从这里取字节免等磁盘）。
/// 路径以 LRU 键（[`LruMap<PathBuf, _>`]）独立存储，条目只保留字节本体。
#[derive(Clone)]
struct ByteEntry {
    bytes: Arc<Vec<u8>>,
}

/// 泛型 LRU 缓存：队首最旧 / 队尾最近用；命中（[`LruMap::get`]）与写入（[`LruMap::put`]）
/// 都移到队尾。条目大小经 `size` 闭包计价并内部维护总字节（[`LruMap::bytes`]）；`put`
/// 按「条目数上限 / 字节上限」从队首淘汰，被替换/被淘汰的条目原样返回（供调用方释放/
/// 记账）。替换原 `cache`（解码结果）与 `byte_cache`（原始字节）两套平行 LRU 的近同构
/// 实现，二者仅 `size` 计价与容量规则不同。
struct LruMap<K, V> {
    /// （键, 值）队列：队首最旧、队尾最近用。
    q: VecDeque<(K, V)>,
    /// 总字节（各条目经 `size` 计价，替换/淘汰同步增减）。
    bytes: usize,
    /// 条目数上限（`None` = 不按张数淘汰，仅字节预算）。
    max_items: Option<usize>,
    /// 总字节上限。
    max_bytes: usize,
    /// 单条目「字节代计」：解码结果用 RGBA 近似内存，字节缓存用 `bytes.len()`。
    size: fn(&V) -> usize,
}

impl<K: PartialEq, V> LruMap<K, V> {
    /// 建空 LRU：`max_items` 为 None 时仅按字节预算淘汰（字节缓存语义）。
    fn new(max_items: Option<usize>, max_bytes: usize, size: fn(&V) -> usize) -> Self {
        Self {
            q: VecDeque::new(),
            bytes: 0,
            max_items,
            max_bytes,
            size,
        }
    }

    fn len(&self) -> usize {
        self.q.len()
    }

    /// 当前总字节（淘汰预算 / 派发停止判断用）。
    fn bytes(&self) -> usize {
        self.bytes
    }

    /// 只读查询（不改变 LRU 顺序）。键按 `Borrow` 匹配：`PathBuf` 键可直接用 `&Path` 查。
    fn contains<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.q.iter().any(|(k, _)| k.borrow() == key)
    }

    /// 命中并移到队尾（MRU），返回可克隆副本（Arc 浅拷贝）；未中 `None`。
    fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: PartialEq + ?Sized,
        V: Clone,
    {
        let pos = self.q.iter().position(|(k, _)| k.borrow() == key)?;
        let (k, v) = self.q.remove(pos).expect("position 已命中");
        let ret = v.clone();
        self.q.push_back((k, v));
        Some(ret)
    }

    /// 写入（同 key 已存在先移除旧条目再入队尾），超出条目数/字节上限按 LRU 从队首淘汰。
    /// 返回被移除 / 被淘汰的全部条目（含同 key 替换的旧条目），供调用方释放 / 记账。
    fn put(&mut self, key: K, value: V) -> Vec<(K, V)> {
        let mut removed = Vec::new();
        if let Some(pos) = self.q.iter().position(|(k, _)| k == &key) {
            let (k, old) = self.q.remove(pos).expect("position 已命中");
            self.bytes = self.bytes.saturating_sub((self.size)(&old));
            removed.push((k, old));
        }
        self.bytes = self.bytes.saturating_add((self.size)(&value));
        self.q.push_back((key, value));
        while self.max_items.is_some_and(|n| self.q.len() > n) || self.bytes > self.max_bytes {
            let Some((k, old)) = self.q.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub((self.size)(&old));
            removed.push((k, old));
        }
        removed
    }

    /// 清空（条目与字节账一并归零）。
    fn clear(&mut self) {
        self.q.clear();
        self.bytes = 0;
    }

    /// 按 LRU 序（队首 → 队尾）迭代值。
    fn iter(&self) -> impl Iterator<Item = &V> {
        self.q.iter().map(|(_, v)| v)
    }
}

impl<K, V> IntoIterator for LruMap<K, V> {
    type Item = V;
    type IntoIter = std::vec::IntoIter<V>;

    fn into_iter(self) -> Self::IntoIter {
        self.q.into_iter().map(|(_, v)| v).collect::<Vec<_>>().into_iter()
    }
}

impl<K, V> Default for LruMap<K, V> {
    /// 空缓存（供 `std::mem::take` / 释放路径使用；`size` 为哑函数，空存时不会被调用）。
    fn default() -> Self {
        Self {
            q: VecDeque::new(),
            bytes: 0,
            max_items: None,
            max_bytes: 0,
            size: |_| 0,
        }
    }
}

/// 图片查看器：自包含的图片浏览视图。
pub struct ImageViewer {
    /// 当前显示的图片路径（标签标题 / 状态栏由此派生）。
    path: PathBuf,
    /// 已解码的图片（`Some` = 可上屏；切换加载期间保留旧图避免闪白）。
    image: Option<Arc<RenderImage>>,
    /// 图片固有像素尺寸（显示比例依据）。
    intrinsic: Option<(f32, f32)>,
    /// 同文件夹兄弟文件导航（清单文件名升序、恒含当前图、恒非空；循环翻页语义收敛在
    /// [`FolderNav`]）。
    nav: FolderNav,
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
    /// 解码结果 LRU 缓存（队首最旧 / 队尾最近用；命中与写入都移到队尾；张数 + 字节双上限）。
    cache: LruMap<PathBuf, CacheEntry>,
    /// 原始文件字节 LRU 缓存（磁盘预读层；解码优先从这里取字节；仅字节预算上限）。
    byte_cache: LruMap<PathBuf, ByteEntry>,
    /// 已在后台解码中的路径去重（当前目标 + 预取共用，防同一路径重复解码）。
    in_flight: HashSet<PathBuf>,
    /// 字节预读在途去重（与解码池分离：IO 便宜，不占 [`MAX_DECODE`] 解码槽）。
    byte_flight: HashSet<PathBuf>,
    /// 解码池并发满时记忆的「最新显示目标」：空位一放立即派发（追最新，跳过中间）。
    pending_target: Option<PathBuf>,
    /// EXIF 嵌入缩略图「即时首帧」任务在途的路径（去重；完成后清空）。临时缩略图只占位
    /// 首帧，真图落盘即被 [`Self::install_current`] 覆盖。
    thumb_pending: Option<PathBuf>,
    /// 像素滚轮累计滚动（px）：超过 [`config::image::WHEEL_PIXEL_STEP`] 才切一张。
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
    /// 上一帧实测的可视视口尺寸（render 期记录；展示像素需要 / 显示尺寸据此）。
    last_viewport: (f32, f32),
    /// 展示像素需要（宽, 高）：由「适应视口尺寸 × zoom」算出，**JPEG 解码档位的下限**——
    /// 解码分辨率 ≥ 该值（GPU 只缩小、不放大）→ 显示不糊。首个 render 前用
    /// [`DISPLAY_NEED_INIT`] 兜底。
    display_need: (f32, f32),
    /// 当前安装展示的 JPEG 降档分母（1/8/4/2/1；非 JPEG 恒 1）。缩放放大且需要更细档时
    /// 据此触发「升档」重解（[`Self::escalate_display_denom`]）。
    display_denom: u8,
    /// 平移（px）：放大后拖拽浏览其他区域，以视口中心为基准、每帧 clamp 到图始终覆盖视口。
    pan: (f32, f32),
    /// 拖拽平移进行中：(左键按下位置, 按下时 pan 起点)。
    drag: Option<((f32, f32), (f32, f32))>,
    /// 「锁定」干净看图模式：隐藏全部非图片 UI、图片充满整个窗口（宿主读取决定外壳取舍）。
    locked: bool,
    /// 底部信息条浮层时序：可见位 + 自隐代序号 + 悬停态 + 延时（收敛于
    /// [`::viewer::auto_hide`]；切图/悬停浮现、1s 自隐、hover 抑制）。
    info_auto: auto_hide::AutoHide,
    /// 当前动图片段（帧切片 + 各帧时长）；`None` = 当前非动图。`install_current` 挂上，
    /// 由 `spawn_anim_clock` 的后台时钟逐帧推进 `self.image`。
    anim_clip: Option<(Vec<Arc<RenderImage>>, Vec<std::time::Duration>)>,
    /// 动图时钟代序号：每次 `install_current`（换图/重装）递增，旧时钟任务据此自停，
    /// 防止旧任务误推进新图帧。
    anim_gen: u64,
    /// 当前动图帧下标（0 = 首帧；配合 `self.image` 落盘当前帧切片）。
    anim_index: usize,
}

impl ImageViewer {
    /// 创建查看器并异步加载 `path`（同文件夹扫描在后台；首图经缓存/worker 流程加载）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let this = Self {
            path: path.clone(),
            image: None,
            intrinsic: None,
            nav: FolderNav::new(vec![path.clone()], 0),
            zoom: 1.0,
            loading: true,
            load_error: None,
            viewport: ScrollHandle::new(),
            bg: None,
            cache: LruMap::new(Some(CACHE_MAX_ITEMS), CACHE_MAX_BYTES, |e: &CacheEntry| e.bytes),
            byte_cache: LruMap::new(None, BYTE_CACHE_BYTES, |e: &ByteEntry| e.bytes.len()),
            in_flight: HashSet::new(),
            byte_flight: HashSet::new(),
            pending_target: None,
            thumb_pending: None,
            scroll_accum: 0.0,
            prefetch_dir: 1,
            prefetch_pos: 1,
            prefetch_origin: 0,
            prefetch_inflight: 0,
            scroll_fast: false,
            last_scroll_dir: 1,
            fast_count: 0,
            last_scroll_at: None,
            last_viewport: (0.0, 0.0),
            display_need: DISPLAY_NEED_INIT,
            display_denom: 1,
            pan: (0.0, 0.0),
            drag: None,
            locked: false,
            info_auto: auto_hide::AutoHide::new(),
            anim_clip: None,
            anim_gen: 0,
            anim_index: 0,
        };
        let scan_at = path.clone();
        cx.spawn(async move |weak, cx| {
            let nav = cx
                .background_executor()
                .spawn(async move { FolderNav::scan(&scan_at, crate::is_image_extension) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.nav = nav;
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
        Some(self.nav.current_path().to_path_buf())
    }

    /// 当前图片在同文件夹清单中的下标（0 基）。
    pub fn current_index(&self) -> usize {
        self.nav.current()
    }

    /// 同文件夹图片总数。
    pub fn folder_len(&self) -> usize {
        self.nav.len()
    }

    /// 是否已加载完成（首张解码成功；仅测试消费）。
    #[cfg(test)]
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.image.is_some()
    }

    /// 是否「锁定」干净看图模式（宿主据此隐藏菜单栏/选项卡栏/状态栏/侧边栏，图片充满窗口）。
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// 切换锁定：锁定 = 隐藏非图片 UI、图片充满窗口（宿主壳体重排后自动缩放填满）；
    /// 解锁回归。切换时平移/拖拽归零（底部信息条始终显示）。
    pub fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        self.locked = !self.locked;
        self.pan = (0.0, 0.0);
        self.drag = None;
        if self.locked {
            self.show_locked_info(cx);
        }
        cx.notify();
    }

    /// 锁定模式：让底部信息条浮现（切图/悬停热区），并在 1 秒后（未悬停时）自隐。
    /// 普通模式（未锁）信息条常驻，此方法为空操作。时序逻辑收敛于
    /// [`auto_hide::AutoHide::show_temporarily`]。
    fn show_locked_info(&mut self, cx: &mut Context<Self>) {
        if !self.locked {
            return;
        }
        self.info_auto
            .show_temporarily(|v| v.locked, |v| &mut v.info_auto, cx);
    }

    /// 底部信息热区悬停（锁定模式，渲染在信息条之上的透明层）：
    /// 进入 → 浮现且不隐藏；移开 → 重新起 1s 自隐。
    fn on_info_zone_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_info_hover(*hovered, cx);
    }

    /// 悬停态设置（独立于窗口，便于测试）：悬停期间不受自动隐藏影响。
    fn set_info_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            self.info_auto.hover_in();
            cx.notify();
        } else {
            self.info_auto.hover_out();
            self.show_locked_info(cx);
        }
    }

    /// 设置画布背景色（宿主按主题传入）。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 当前画布背景色（`set_background` 传入；None = 未设置 → render 回落主题 canvas）。
    pub fn background(&self) -> Option<Hsla> {
        self.bg
    }

    /// 释放已上屏与缓存的 GPU 纹理（关闭标签页时调用，与 PDF 阅读器同款）。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(img) = self.image.take() {
            cx.drop_image(img, Some(window));
        }
        for entry in std::mem::take(&mut self.cache).into_iter() {
            cx.drop_image(entry.render, Some(window));
        }
        self.byte_cache.clear();
        self.in_flight.clear();
        self.byte_flight.clear();
        self.pending_target = None;
        self.thumb_pending = None;
        self.prefetch_inflight = 0;
        self.display_denom = 1;
        self.display_need = DISPLAY_NEED_INIT;
        self.pan = (0.0, 0.0);
        self.drag = None;
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

    /// 适应视口（Contain，**贴边无留白**）基准尺寸（不含缩放）：取较小缩放比，两轴任意
    /// 一端**正好贴住窗口上/下或左/右边缘**、保持原始比例（另一端居中留白）。
    /// 逻辑收敛在 [`pan_zoom::contain_fit`]（image/video 共用同一几何）。
    fn contain_fit(iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        pan_zoom::contain_fit(iw, ih, vw, vh)
    }

    /// 显示尺寸 = 适应视口（Contain，贴边）× 缩放倍数。
    fn display_size(&self, iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        let (fw, fh) = Self::contain_fit(iw, ih, vw, vh);
        (fw * self.zoom, fh * self.zoom)
    }

    /// 刷新「展示像素需要」[`Self::display_need`]（= 适应视口尺寸 × zoom）：它就是 JPEG
    /// 解码档位的**下限**——解码分辨率 ≥ 它，GPU 只会缩小不会放大，显示不糊、内存不浪费。
    /// render 期与缩放/切图事件期调用；视口未布局（首帧）时保持 [`DISPLAY_NEED_INIT`]。
    fn update_display_need(&mut self) {
        let Some((iw, ih)) = self.intrinsic else {
            self.display_need = DISPLAY_NEED_INIT;
            return;
        };
        let (vw, vh) = self.last_viewport;
        let (dw, dh) = Self::contain_fit(iw, ih, vw, vh);
        self.display_need = ((dw * self.zoom).max(1.0), (dh * self.zoom).max(1.0));
    }

    /// 画布默认底色：亮 #F5F5F5 / 暗 #202020 中性灰阶（与外壳 `window_bg` 同源；与 PDF 阅读器一致）。
    fn default_canvas(cx: &App) -> Hsla {
        let dark = ui::theme::Theme::global(cx).is_dark();
        ui::theme::ChromeColors::for_mode(dark).window_bg.into()
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
        let delta_y = pan_zoom::wheel_delta_y(event);
        if delta_y == 0.0 {
            return;
        }
        if event.modifiers.control {
            // 实测本平台：向上滚动 delta.y>0 → 放大；向下 <0 → 缩小（与编辑器字号缩放一致）
            self.adjust_zoom(pan_zoom::zoom_factor(delta_y, ZOOM_STEP), cx);
        } else {
            // 单滚轮：未放大 → 切图（向下滚 → 下一张，向上滚 → 上一张）；放大态 →
            // 先在图片内平移，滚到边缘继续同向外滚才切图（[`Self::wheel_pan_or_switch`]）。
            match event.delta {
                // 行滚轮（鼠标）：1 事件 = 1 格（放大态平移一格 = 视口高的一部分）。
                ScrollDelta::Lines(_) => {
                    let (_, vh) = self.last_viewport;
                    let pan_step = (vh * WHEEL_LINE_PAN_FRACTION).max(32.0);
                    self.wheel_pan_or_switch(if delta_y < 0.0 { 1 } else { -1 }, pan_step, cx);
                }
                // 像素滚轮（触控板）：累计像素，超过一步门限才动作（未放大切图时合并
                // 洪水事件；放大态按步进平移，可连续跟随）。
                ScrollDelta::Pixels(_) => {
                    self.scroll_accum += delta_y;
                    let step = config::image::WHEEL_PIXEL_STEP;
                    let pan_step = step * WHEEL_PIXEL_PAN_MULT;
                    while self.scroll_accum >= step {
                        self.wheel_pan_or_switch(1, pan_step, cx);
                        self.scroll_accum -= step;
                    }
                    while self.scroll_accum <= -step {
                        self.wheel_pan_or_switch(-1, pan_step, cx);
                        self.scroll_accum += step;
                    }
                }
            }
        }
        cx.stop_propagation();
    }

    /// 滚轮分发（未放大 ↔ 放大态切换语义）：
    /// - **未放大**（图两轴都 ≤ 视口）→ 保持原有「滚轮切图」：`dir=1`（向下滚）切下一张、
    ///   `dir=-1` 切上一张；
    /// - **放大态** → 优先**纵向平移**（`dir=1` = 看图片下方，`pan.y -= pan_step`），滚到该轴
    ///   边缘且**继续同向外滚**（越界量）才触发切图——即「滚到底部才切下一张」；纵向不越界
    ///   时才启用横向平移。切图由 [`Self::switch`] 统一复位（缩放→适应视口、平移/拖拽归零）。
    fn wheel_pan_or_switch(&mut self, dir: i32, pan_step: f32, cx: &mut Context<Self>) {
        let (vw, vh) = self.last_viewport;
        if vw <= 0.0 || vh <= 0.0 {
            // 视口未知（未布局）：退回切图语义
            self.switch(dir, cx);
            return;
        }
        let (dw, dh) = self
            .intrinsic
            .map(|(iw, ih)| self.display_size(iw, ih, vw, vh))
            .unwrap_or((0.0, 0.0));
        let ay = (dh - vh).max(0.0) * 0.5;
        let ax = (dw - vw).max(0.0) * 0.5;
        if ay <= 0.0 && ax <= 0.0 {
            self.switch(dir, cx);
            return;
        }
        let outward = if ay > 0.0 { &mut self.pan.1 } else { &mut self.pan.0 };
        let limit = if ay > 0.0 { ay } else { ax };
        let inc = if dir > 0 { -pan_step } else { pan_step };
        let next = *outward + inc;
        let clamped = next.clamp(-limit, limit);
        *outward = clamped;
        cx.notify();
        let past_edge = if dir > 0 {
            clamped <= -limit && next < -limit
        } else {
            clamped >= limit && next > limit
        };
        if past_edge {
            self.switch(dir, cx);
        }
    }

    /// 应用缩放因子并约束在 [ZOOM_MIN, ZOOM_MAX]（钳位/无变化短路由 [`pan_zoom::apply_zoom`]）。
    /// 缩放会改变「展示像素需要」（放大需更细档），故刷新 [`Self::display_need`] 并在
    /// 「当前展示比需要更粗」时升档重解。
    fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if !pan_zoom::apply_zoom(&mut self.zoom, factor, ZOOM_MIN, ZOOM_MAX) {
            return;
        }
        self.update_display_need();
        cx.notify();
        self.escalate_display_denom(cx);
    }

    /// 在同文件夹清单内切换（`dir` = ±1，环绕语义收敛于 [`FolderNav::step`]）。切换本身
    /// O(1)：目标立即写入 `path`（标题/状态栏跟着走），显示由 `request_display` 决定「缓存
    /// 命中秒切 / 未中池解码追最新」。方向变化时重置级联预取锚点；同时做 B4「快速连续
    /// 滚动」判定（连续同向且步间隔 ≤ [`FAST_SCROLL_STEP_MS`] 累计到阈值 → 临时放宽前向窗口）。
    fn switch(&mut self, dir: i32, cx: &mut Context<Self>) {
        let Some(next) = self.nav.step(i64::from(dir)) else {
            return;
        };
        if next == self.nav.current() {
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
        let _ = self.nav.set_current(next);
        self.path = self.nav.item(next).expect("switch 目标恒在清单内");
        // 切图：缩放复位为适应视口 + 平移/拖拽归零（每张都从初始大小开始）
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.drag = None;
        self.request_display(cx);
        // 锁定模式：切图 → 底部信息条浮现 1s
        self.show_locked_info(cx);
    }

    /// 显示入口（首图 / 切换共用）：`path` 已指向目标。缓存命中 → 瞬时落盘并启动级联
    /// 预取；未中 → 置 loading 并进入「并行解码池」（去重 / 追最新）。命中后按当前
    /// 展示需要判断是否需升档（缓存的可能是旧档位）。
    fn request_display(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        if let Some(entry) = self.cache_get(&path) {
            self.install_current(entry, &path, cx);
            self.escalate_display_denom(cx);
            return;
        }
        self.loading = true;
        self.load_error = None;
        // P1b「即时首帧」：冷目标若是 JPEG，先解出 EXIF 内嵌缩略图立即上屏（<5ms），
        // 展示档真图解码完成后再覆盖——冷打开首帧不再空等熵解码。
        self.maybe_show_exif_thumb(&path, cx);
        self.queue_decode(path, true, cx);
    }

    /// P1b：EXIF 嵌入缩略图「即时首帧」（零依赖；school 等手机照片在 APP1-Exif 的 IFD1 常嵌
    /// 一张 ~160px 缩略 JPEG）。冷目标且是 JPEG 时派发：后台只读**文件头**（EXIF 段及其内嵌
    /// 缩略图都位于头部，APP1 长度 16 位 ≤64KB，读 256KB 足够）→ 提取 → 解码 → 装成**临时
    /// 占位**上屏。占位**不**升档（`display_denom` 置 1 短路 [`Self::escalate_display_denom`]）、
    /// **不**重锚级联预取、**不**进 LRU——这些等真图 [`Self::install_current`] 再做（打开首图
    /// 仍独占解码槽不抢跑）。`loading` 保持 true：真图未就绪前才够资格占位。
    fn maybe_show_exif_thumb(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !path_is_jpeg(path) || self.thumb_pending.as_deref() == Some(path) {
            return;
        }
        let cached = self.byte_cache_get(path);
        self.thumb_pending = Some(path.to_path_buf());
        let target = path.to_path_buf();
        let probe = target.clone();
        cx.spawn(async move |weak, cx| {
            let thumb = cx
                .background_executor()
                .spawn(async move {
                    let head = match cached {
                        Some(b) => Some(b[..b.len().min(EXIF_PROBE_BYTES)].to_vec()),
                        None => read_head(&probe, EXIF_PROBE_BYTES),
                    };
                    head.and_then(|h| extract_exif_jpeg_thumbnail(&h))
                        .and_then(|tb| decode_jpeg_display(&tb, DISPLAY_NEED_INIT).ok())
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.thumb_pending.as_deref() == Some(target.as_path()) {
                    this.thumb_pending = None;
                }
                if let Some((bgra, w, h, den)) = thumb
                    && this.path == target
                    && this.loading
                    && let Some(frame) = bgra_single_frame(bgra, w, h)
                    && let Some(entry) =
                        Self::build_entry(target.clone(), vec![frame], w, h, den)
                {
                    this.install_provisional(entry, cx);
                }
            });
        })
        .detach();
    }

    /// 把临时占位（EXIF 缩略图）上屏：只替换 `image`/`intrinsic`，并置 `display_denom=1`
    /// （临时小图不触发升档）。不动 `loading`、不重锚预取、不进 LRU——真图
    /// [`Self::install_current`] 会覆盖这一切。
    fn install_provisional(&mut self, entry: CacheEntry, cx: &mut Context<Self>) {
        self.image = Some(entry.render);
        self.intrinsic = Some(entry.intrinsic);
        self.display_denom = 1;
        cx.notify();
        cx.refresh_windows();
    }

    // ---- 放大后拖拽平移（鼠标左键抓图滑动浏览其他区域） ----

    /// 按下左键开始拖拽：记录起点鼠标位置 + 当前 `pan`（未放大时 clamp 恒 0，拖动无效果）。
    fn on_pan_down(
        &mut self,
        ev: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.button == MouseButton::Left {
            self.drag = Some(pan_zoom::pan_drag_begin(
                (ev.position.x.as_f32(), ev.position.y.as_f32()),
                self.pan,
            ));
            cx.notify();
        }
    }

    /// 拖拽平移：按「当前鼠标 - 起点」增量更新 `pan`，clamp 到图始终覆盖视口。
    fn on_pan_move(
        &mut self,
        ev: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        let pos = (ev.position.x.as_f32(), ev.position.y.as_f32());
        let (dx, dy) = pan_zoom::pan_drag_delta(&drag, pos);
        let (vw, vh) = self.last_viewport;
        let (dw, dh) = self
            .intrinsic
            .map(|(iw, ih)| self.display_size(iw, ih, vw, vh))
            .unwrap_or((0.0, 0.0));
        self.pan = Self::clamp_pan((drag.1.0 + dx, drag.1.1 + dy), vw, vh, dw, dh);
        cx.notify();
    }

    /// 拖拽结束（窗口内/外松开统一清态；`on_mouse_up` / `on_mouse_up_out` 共用）。
    fn on_pan_up(&mut self, _ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.drag = None;
            cx.notify();
        }
    }

    /// 平移 clamp：放大后（`dw>vw` 或 `dh>vh`）允许在图内滑动，范围 = ±(图-视口)/2；
    /// 未放大时范围 0（永远居中，拖拽无效果）。以「视口中心 = 图中心」为 0 基准，
    /// 缩放/切图后范围变化由 render 期逐帧重新 clamp 收敛。
    /// 逻辑收敛在 [`pan_zoom::clamp_pan`]（image/video 共用）。
    fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
        pan_zoom::clamp_pan(pan, vw, vh, dw, dh)
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
    /// 字节回填缓存（重解码不重读盘）。按当前「展示像素需要」解码（JPEG 走 libjpeg-turbo
    /// SIMD 降档直出 BGRA；其它格式走 `image` 全解）。结果只进缓存；仅当「仍是当前显示
    /// 目标」才上屏。
    fn spawn_decode(&mut self, path: PathBuf, is_prefetch: bool, cx: &mut Context<Self>) {
        self.in_flight.insert(path.clone());
        if is_prefetch {
            self.prefetch_inflight += 1;
        }
        let bytes = self.byte_cache_get(&path);
        let decode_path = path;
        let read_path = decode_path.clone();
        let need = self.display_need;
        cx.spawn(async move |weak, cx| {
            // 后台：取字节（缓存命中直接用；未中读盘）→ 按展示需要解码 → 返回 (原始字节, 结果)。
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let raw: Arc<Vec<u8>> = match bytes {
                        Some(b) => b,
                        None => Arc::new(std::fs::read(&read_path).map_err(|e| e.to_string())?),
                    };
                    let decoded = decode_background(&raw, need);
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
                            ByteEntry { bytes: raw },
                        );
                        match decoded {
                            Ok((frames, w, h, den)) => {
                                if let Some(entry) =
                                    Self::build_entry(decode_path.clone(), frames, w, h, den)
                                {
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
    /// [`Self::queue_decode`] 自行处理）。
    fn drain_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.pending_target.take() {
            self.queue_decode(p, true, cx);
        }
    }

    /// 「升档」：当前展示是 JPEG 且降档分母比当前展示需要更粗（解码像素比显示小）时，
    /// 重解更细档替换。直接派发（跳过 [`Self::queue_decode`] 的缓存短路——缓存里存的旧档位
    /// 命中只会装回旧图）；`in_flight`/并发满则记 [`Self::pending_target`]。完成后新档
    /// 经 `cache_put` 替换粗档并上屏，`display_denom` 收敛到 ≤ 需要 → 不再触发（只升不降）。
    fn escalate_display_denom(&mut self, cx: &mut Context<Self>) {
        if self.display_denom == 1 || !path_is_jpeg(&self.path) {
            return;
        }
        let Some((iw, ih)) = self.intrinsic else {
            return;
        };
        let desired = pick_jpeg_denom(iw as u32, ih as u32, self.display_need);
        if self.display_denom <= desired {
            return;
        }
        let path = self.path.clone();
        if self.in_flight.contains(&path) {
            return;
        }
        if self.in_flight.len() >= MAX_DECODE {
            self.pending_target = Some(path);
            return;
        }
        self.spawn_decode(path, false, cx);
    }

    /// 从 BGRA 帧序列构建缓存条目（`denom` 记入条目：展示档位 / 升档判定用）。帧序列
    /// 必须横向统一尺寸（`w×h`）——GIF 解码已按逻辑屏画布统一；`bytes` 按
    /// 「单帧字节 × 帧数」诚实记账：大 GIF 会如实算成块头、超 [`config::image::CACHE_MAX_BYTES`]
    /// 预算时被 LRU 立刻淘汰——可接受（动画帧必须全量驻留才能播，口径如实反映）。
    fn build_entry(
        path: PathBuf,
        frames: Vec<Frame>,
        w: u32,
        h: u32,
        denom: u8,
    ) -> Option<CacheEntry> {
        if frames.is_empty() {
            return None;
        }
        let bytes = (w as usize * h as usize * 4).checked_mul(frames.len())?;
        // 逐帧切片成**独立单帧** RenderImage（详见 [`CacheEntry::gif`]：绕开 gpui `img()`
        // 只在窗口聚焦时自动动画的限制，改由本查看器时钟驱动）。首帧同时作为 `render`
        // （静态预览/占位）；帧 buffer 随 Frame 移入各自 RenderImage，不额外复制内存。
        let mut per_frame: Vec<Arc<RenderImage>> = Vec::with_capacity(frames.len());
        let mut delays: Vec<std::time::Duration> = Vec::with_capacity(frames.len());
        for frame in frames {
            delays.push(std::time::Duration::from(frame.delay()));
            per_frame.push(Arc::new(RenderImage::new(vec![frame])));
        }
        let render = per_frame[0].clone();
        Some(CacheEntry {
            path,
            render,
            intrinsic: (w as f32, h as f32),
            bytes,
            denom,
            gif: (per_frame.len() > 1).then(|| (per_frame, delays)),
        })
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
        let n = self.nav.len() as i64;
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
            // 解码缓存已满（字节预算）→ 预取一解即被 LRU 挤掉，纯白算 CPU：停止新派发
            if self.cache.bytes() >= CACHE_MAX_BYTES {
                break;
            }
            let ni = self.prefetch_pos.rem_euclid(n) as usize;
            let Some(path) = self.nav.item(ni) else {
                break;
            };
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
            let base = self.nav.current() as i64;
            for k in 1..=PREFETCH_BACKWARD as i64 {
                if self.prefetch_inflight + 1 >= MAX_DECODE
                    || self.cache.len() >= CACHE_MAX_ITEMS
                    || self.cache.bytes() >= CACHE_MAX_BYTES
                {
                    break;
                }
                let ni = (base - k).rem_euclid(n) as usize;
                let Some(path) = self.nav.item(ni) else {
                    break;
                };
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
                let Some(path) = self.nav.item(ni) else {
                    break;
                };
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
                    this.byte_cache_put(p.clone(), ByteEntry { bytes: b });
                }
            });
        })
        .detach();
    }

    /// 把缓存条目落盘为当前显示 + 通知宿主 + 重锚级联预取（窗口沿方向滑动更新）。
    fn install_current(&mut self, entry: CacheEntry, path: &Path, cx: &mut Context<Self>) {
        self.path = path.to_path_buf();
        self.image = Some(entry.render.clone());
        self.intrinsic = Some(entry.intrinsic);
        self.display_denom = entry.denom;
        // 动图：挂上帧切片并启动后台时钟（详见 [`Self::spawn_anim_clock`]；reduce_motion
        // 时保持静态首帧，尊重「减少动态效果」设置）。换图/重装必先递增代序号自停旧时钟。
        self.anim_gen = self.anim_gen.wrapping_add(1);
        let anim = entry
            .gif
            .as_ref()
            .is_some_and(|(f, _)| f.len() > 1)
            && !cx.reduce_motion();
        self.anim_clip = if anim { entry.gif.clone() } else { None };
        self.anim_index = 0;
        self.update_display_need();
        self.loading = false;
        self.load_error = None;
        cx.notify();
        cx.refresh_windows();
        if anim {
            self.spawn_anim_clock(cx);
        }
        // 锚点移到当前位置：前向/反向窗口随位置滑动，继续沿方向补满
        self.prefetch_origin = self.nav.current() as i64;
        self.prefetch_pos = self.nav.current() as i64 + self.prefetch_dir as i64;
        self.pump_prefetch(cx);
    }

    /// 后台动画时钟：按各帧时长逐帧推进当前动图（`self.image` = 当前帧单帧 `RenderImage`），
    /// 并 `notify`+`refresh_windows` 触发重绘——由本查看器驱动，**不依赖窗口焦点**（gpui
    /// `img()` 自动动画只在窗口聚焦时推进，失焦即冻结，故绕开它）。
    /// 以 `anim_gen` 代序号防旧任务误更新：换图/重装后 `install_current` 递增代序号，
    /// 旧任务在下一轮更新时发现不匹配即自停；实体销毁后 `update` 失败自停。
    fn spawn_anim_clock(&mut self, cx: &mut Context<Self>) {
        let clock_gen = self.anim_gen;
        let Some((frames, delays)) = self.anim_clip.clone() else {
            return;
        };
        let n = frames.len();
        if n <= 1 {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut idx = 0usize;
            loop {
                let wait = delays
                    .get(idx)
                    .copied()
                    .unwrap_or(std::time::Duration::from_millis(100));
                cx.background_executor().timer(wait).await;
                let alive = this.update(cx, |this, cx| {
                    if this.anim_gen != clock_gen {
                        return false;
                    }
                    idx = (idx + 1) % n;
                    this.anim_index = idx;
                    this.image = Some(frames[idx].clone());
                    cx.notify();
                    cx.refresh_windows();
                    true
                });
                if alive.map_or(true, |v| !v) {
                    return; // 换图 / 实体销毁 → 自停
                }
            }
        })
        .detach();
    }

    /// 缓存查询并移到队尾（MRU）。命中返回克隆（Arc 浅拷贝），未中 `None`。
    fn cache_get(&mut self, path: &Path) -> Option<CacheEntry> {
        self.cache.get(path)
    }

    /// 缓存是否已含某路径（只读，不改变 LRU 顺序）。
    fn cache_contains(&self, path: &Path) -> bool {
        self.cache.contains(path)
    }

    /// 写入缓存（已存在则移到队尾），超出张数/字节预算按 LRU 从队首淘汰。
    fn cache_put(&mut self, path: PathBuf, entry: CacheEntry) {
        let _ = self.cache.put(path, entry);
    }

    /// 字节缓存查询并移到队尾（MRU），返回 Arc 克隆（保留在缓存里供解码/重解码复用）。
    fn byte_cache_get(&mut self, path: &Path) -> Option<Arc<Vec<u8>>> {
        self.byte_cache.get(path).map(|e| e.bytes)
    }

    /// 字节缓存是否已含某路径（只读，不改变 LRU 顺序）。
    fn byte_cache_contains(&self, path: &Path) -> bool {
        self.byte_cache.contains(path)
    }

    /// 写入字节缓存（已存在则移到队尾），超出字节预算 [`BYTE_CACHE_BYTES`] 按 LRU 淘汰。
    fn byte_cache_put(&mut self, path: PathBuf, entry: ByteEntry) {
        let _ = self.byte_cache.put(path, entry);
    }

    /// C6：取滚动方向最近一张「已解码在缓存、但非当前显示」的图，包成**垫底透明 img**
    /// （同尺寸、opacity 0、不拦截输入）。让 gpui 在上屏前就完成该纹理的 GPU 上传，切到
    /// 的那一帧不再肩负首次上传（真实 GPU 生效；headless 测试无真实纹理，仅保证构造正确）。
    fn peek_upgrade_element(&self) -> Option<gpui::AnyElement> {
        let n = self.nav.len() as i64;
        if n <= 1 {
            return None;
        }
        let ni = (self.nav.current() as i64 + self.prefetch_dir as i64).rem_euclid(n) as usize;
        let target = self.nav.get(ni)?;
        if *target == self.path {
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

/// JPEG 魔数（FF D8 FF）检测：区分「JPEG 走 libjpeg-turbo 降档」与「其它格式走 image 全解」。
/// 以字节为准，比扩展名可信（伪装成 .jpg 的其它格式不会被误当 JPEG 硬解）。
fn is_jpeg_magic(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// GIF 魔数（`GIF8`）检测：GIF 走全帧动图解码，其余格式单帧静态。以字节为准，
/// 伪装成其它扩展名的 GIF 也会被识别。
fn is_gif_magic(b: &[u8]) -> bool {
    b.starts_with(b"GIF")
}

/// 路径扩展名是否 jpg/jpeg（升档路由的廉价判据；最终仍以魔数 + `probe` 为准）。
fn path_is_jpeg(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// EXIF APP1 探读上限（字节）：EXIF 段及其内嵌缩略图都位于文件头（APP1 长度 16 位、
/// ≤64KB），读头即可提取，不必全量读盘。
const EXIF_PROBE_BYTES: usize = 256 * 1024;

/// 读文件头前 `n` 字节（免全量读盘；EXIF 提取只需头部）。
fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(n.min(1 << 20));
    f.take(n as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// 从 JPEG 的 EXIF APP1（TIFF IFD1 的 0x0201/0x0202）提取嵌入缩略图 JPEG 字节。
///
/// 手机照片（school 已实测）在 APP1-Exif 的 TIFF IFD1 常嵌一张 ~160px 缩略 JPEG——冷打开
/// 先显它（几 ms）再让展示档真图替换，消除「首帧空等熵解码」。仓库依赖 `libjpeg-turbo-rs`
/// 的 `extract_embedded_jpeg` 只解 **TIFF-RAW** 容器、`extract_jfif_thumbnail` 只解 **APP0-JFIF**，
/// 都不读 JPEG 的 EXIF APP1，故自实现最小「APP 段扫描 + TIFF-IFD 遍历」（纯字节、零依赖）。
fn extract_exif_jpeg_thumbnail(data: &[u8]) -> Option<Vec<u8>> {
    let mut i = 2usize; // 跳过 SOI（FFD8）
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        match marker {
            // 无长度段的独立标记（TEM / RSTn）：直接跳 2 字节
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            // SOS / EOI：段扫描到此结束；仍未见到 EXIF APP1 → 无缩略图
            0xDA | 0xD9 => return None,
            _ => {}
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        if marker == 0xE1 {
            let payload = &data[(i + 4)..(i + 2 + len)];
            if payload.starts_with(b"Exif\0\0") {
                return exif_ifd_thumbnail(&payload[6..]);
            }
        }
        i += 2 + len;
    }
    None
}

/// 在 APP1 的 TIFF 载荷里沿 IFD 链找 0x0201(JPEGInterchangeFormat)/0x0202(JPEGInterchangeFormatLength)
/// 并切片出缩略 JPEG（校验 SOI 魔数）。同时支持小端「II」/ 大端「MM」；偏移以 TIFF 头起点为基准。
fn exif_ifd_thumbnail(tiff: &[u8]) -> Option<Vec<u8>> {
    if tiff.len() < 8 {
        return None;
    }
    let le = match (tiff[0], tiff[1]) {
        (0x49, 0x49) => true, // "II" 小端
        (0x4D, 0x4D) => false, // "MM" 大端
        _ => return None,
    };
    let u16at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    if u16at(2)? != 42 {
        return None; // TIFF 魔数不符
    }
    let mut ifd = u32at(4)? as usize;
    for _ in 0..8 {
        let n = u16at(ifd)? as usize;
        let entries = ifd + 2;
        let end = entries.checked_add(n * 12)?;
        if end > tiff.len() {
            return None;
        }
        let mut jpeg_off: Option<usize> = None;
        let mut jpeg_len: Option<usize> = None;
        for k in 0..n {
            let e = entries + k * 12;
            match u16at(e)? {
                0x0201 => jpeg_off = u32at(e + 8).map(|v| v as usize),
                0x0202 => jpeg_len = u32at(e + 8).map(|v| v as usize),
                _ => {}
            }
        }
        if let (Some(o), Some(l)) = (jpeg_off, jpeg_len)
            && l > 0
            && o.checked_add(l).is_some_and(|x| x <= tiff.len())
        {
            let blob = &tiff[o..o + l];
            return blob.starts_with(&[0xFF, 0xD8, 0xFF]).then(|| blob.to_vec());
        }
        let next = u32at(end)? as usize;
        if next == 0 {
            return None;
        }
        ifd = next;
    }
    None
}

/// 按「展示像素需要」选 JPEG 降档分母 {1,2,4,8}：取**最大**的、让解码尺寸 ≥ 需要尺寸的
/// 分母（缺省 1 = 全解）。保证一张图只在「解码 ≥ 显示」的前提下尽量省内存——GPU 只会把
/// 图缩小、绝不放大 → 显示**不糊**（这是行业「decode to display size」的地基：放大时
/// 需要涨、分母自然下降，需要 > 原图时才落到全解）。
fn pick_jpeg_denom(iw: u32, ih: u32, need: (f32, f32)) -> u8 {
    let (nw, nh) = (need.0.max(1.0), need.1.max(1.0));
    for den in [8u8, 4, 2, 1] {
        let w = (iw as f32 / den as f32).ceil();
        let h = (ih as f32 / den as f32).ceil();
        if w >= nw && h >= nh {
            return den;
        }
    }
    1
}

/// JPEG 按展示需要**降档解码**（libjpeg-turbo SIMD，IDCT 缩放 + 直出 BGRA，无中间拷贝）：
/// 返回 BGRA 字节 + 输出尺寸 + 实际降档分母。`probe` 先读头拿完整尺寸再选档。
fn decode_jpeg_display(
    bytes: &[u8],
    need: (f32, f32),
) -> Result<(Vec<u8>, u32, u32, u8), String> {
    let info = libjpeg_turbo_rs::probe(bytes)
        .map_err(|e| format!("JPEG 读取头部失败：{e}"))?;
    let (iw, ih) = (info.width as u32, info.height as u32);
    let den = pick_jpeg_denom(iw, ih, need);
    let mut dec = LtjDecoder::new(bytes).map_err(|e| format!("JPEG 解码器初始化失败：{e}"))?;
    dec.set_scale(ScalingFactor::new(1, u32::from(den)));
    dec.set_output_format(LtjPixelFormat::Bgra);
    let img = dec
        .decode_image()
        .map_err(|e| format!("JPEG 解码失败：{e}"))?;
    Ok((img.data, img.width as u32, img.height as u32, den))
}

/// RGBA → BGRA（原地交换每像素 R/B；缓冲区按 w×h×4 的 RGBA 直排）。GPUI 的
/// `RenderImage` 字节序为 BGRA（PDF 模块亦如此）；不交换会红蓝互换、颜色明显失真。
fn swap_rb_in_place(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

/// BGRA 像素包成单帧（JPEG 降档直出 / EXIF 缩略图等单帧路径复用入口）。
fn bgra_single_frame(bgra: Vec<u8>, w: u32, h: u32) -> Option<Frame> {
    Some(Frame::new(RgbaImage::from_raw(w, h, bgra)?))
}

/// 单帧静态解码：`image` 全解取首帧 → **BGRA** 帧序列（GIF 异常回退等独立路径用，
/// 不递归 GifDecoder）。
fn decode_static_first_frame(bytes: &[u8]) -> Result<(Vec<Frame>, u32, u32), String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("无法解码图片：{e}"))?;
    let mut rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    swap_rb_in_place(rgba.as_mut());
    Ok((vec![Frame::new(rgba)], w, h))
}

/// GIF 全帧解码失败 → 回退「首帧静态」（与改前 behaviour 一致，绝不硬失败）；静态
/// 也解不动时返回原始 GIF 错误（顶层仍以加载错误呈现，不 panic）。
fn decode_static_first_frame_or(
    bytes: &[u8],
    gif_err: image::ImageError,
) -> Result<(Vec<Frame>, u32, u32), String> {
    match decode_static_first_frame(bytes) {
        Ok(out) => Ok(out),
        Err(_) => Err(format!("无法解码图片：{gif_err}")),
    }
}

/// GIF 全帧解码 → **BGRA** 帧序列：`GifDecoder::into_frames` 逐帧解出全部帧并保留每帧
/// 自身 delay。image 0.25 的 GIF 解码器已把分块/局部帧按 (left,top) 合成到逻辑屏画布、
/// 处理 disposal（Keep/Background/Previous），产出的帧**一律逻辑屏尺寸、统一大小**——
/// `RenderImage` 的 frame_index 切换正好假设帧间同尺寸，无需自造合成。帧数超
/// [`MAX_GIF_FRAMES`] 只保留前 N 帧（动画仍循环）。资源护栏：
/// 1. 解码器套上 image 默认 `Limits`（max_alloc=512MB）——`GifDecoder::new` 内部是
///    `Limits::no_limits()`，直接 `into_frames` 会在首个帧前就按逻辑屏画布预分配无上限
///    的 RGBA 画布（荒谬超大逻辑屏描述符可瞬间吃爆内存）；limits 在 reserve 阶段用
///    512MB 单分配预算拦下，超额以 `ImageResult` 报错，不 panic；
/// 2. 帧尺寸与逻辑屏不一致（异常文件）→ 整体回退「首帧静态」；
/// 3. 全帧解码失败 → [`decode_static_first_frame_or`] 回退「首帧静态」，绝不硬失败。
fn decode_gif_frames(bytes: &[u8]) -> Result<(Vec<Frame>, u32, u32), String> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map_err(|e| format!("无法解码图片：{e}"))?;
    // 资源护栏①：套上 image 默认 Limits（max_alloc=512MB）——`GifDecoder::new` 内部是
    // `Limits::no_limits()`，直接 `into_frames` 会在首个帧前就按逻辑屏画布预分配无上限
    // 的 RGBA 画布（荒谬超大逻辑屏描述符可瞬间吃爆内存）；默认 limits 在 `reserve` 阶段
    // 用 512MB 单分配预算拦下，超额以 `ImageResult` 报错，不 panic。
    decoder
        .set_limits(image::Limits::default())
        .map_err(|e| format!("无法解码图片：{e}"))?;
    let (w, h) = decoder.dimensions();
    let frames = match decoder
        .into_frames()
        .take(MAX_GIF_FRAMES)
        .collect::<image::ImageResult<Vec<_>>>()
    {
        Ok(frames) => frames,
        Err(e) => return decode_static_first_frame_or(bytes, e),
    };
    if frames.is_empty() {
        return Err("无法解码图片：GIF 无帧".into());
    }
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        let (fw, fh) = frame.buffer().dimensions();
        if fw != w || fh != h {
            return decode_static_first_frame(bytes);
        }
        let delay = frame.delay();
        let mut buf = frame.into_buffer();
        swap_rb_in_place(buf.as_mut());
        out.push(Frame::from_parts(buf, 0, 0, delay));
    }
    Ok((out, w, h))
}

/// 非 JPEG 全解 → **BGRA** 帧序列（`image` crate 默认特性支持 png/gif/bmp/webp/ico/tiff…）。
/// GIF 走全帧动图（[`decode_gif_frames`]）；其余格式单帧静态（[`decode_static_first_frame`]）。
/// 返回 (帧序列, 逻辑宽, 逻辑高)——所有帧统一为逻辑屏画布尺寸、BGRA 字节序。
fn decode_generic(bytes: &[u8]) -> Result<(Vec<Frame>, u32, u32), String> {
    if is_gif_magic(bytes) {
        decode_gif_frames(bytes)
    } else {
        decode_static_first_frame(bytes)
    }
}

/// 后台解码总入口：JPEG → libjpeg-turbo 按展示需要降档直出 BGRA（特殊 JPEG 如 CMYK / 12bit /
/// lossless 解不动时回退 `image` 全解，绝不硬失败）包成单帧；非 JPEG → `image` 全解
/// （GIF 解全帧动图）。返回 (BGRA 帧序列, 宽, 高, 降档分母)。
fn decode_background(bytes: &[u8], need: (f32, f32)) -> Result<(Vec<Frame>, u32, u32, u8), String> {
    if is_jpeg_magic(bytes) {
        match decode_jpeg_display(bytes, need) {
            Ok((bgra, w, h, den)) => {
                let frame = bgra_single_frame(bgra, w, h).ok_or("解码结果尺寸异常")?;
                Ok((vec![frame], w, h, den))
            }
            Err(_) => {
                let (frames, w, h) = decode_generic(bytes)?;
                Ok((frames, w, h, 1))
            }
        }
    } else {
        let (frames, w, h) = decode_generic(bytes)?;
        Ok((frames, w, h, 1))
    }
}

/// 读盘 + 全分辨率解码（旧链路；仅测试 / bench 对比「全解基准值」用，生产走
/// [`decode_background`]）。
#[cfg(test)]
fn decode_image_at(path: &Path) -> Result<(Vec<Frame>, u32, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_generic(&bytes)
}

/// 读盘 + 按展示需要降档解码（bench / 测试验证「显示尺寸档」用）。
#[cfg(test)]
fn decode_display_at(path: &Path, need: (f32, f32)) -> Result<(Vec<Frame>, u32, u32, u8), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_background(&bytes, need)
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
        let (frames, w, h) = decode_image_at(&p).expect("自产 PNG 应能解码");
        assert_eq!((w, h), (40, 20));
        assert_eq!(frames.len(), 1, "PNG 应为单帧");
        let bgra = frames[0].buffer().as_raw();
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

    /// 编码一张 N 帧测试 GIF（每帧统一 4×3 逻辑屏全画布；(0,0) 像素为 `color`、帧 delay 为
    /// `ms`）。用 `GifEncoder`（image 0.25 默认特性含 gif 编码）。GIF 编码 delay 精度
    /// 10ms（写盘时除 10 取整），测试参数须用 10 的倍数才能精确往返。
    fn encode_test_gif(frames: &[(image::Rgba<u8>, u32)]) -> Vec<u8> {
        let (w, h) = (4u32, 3u32);
        let mut out = Vec::new();
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        for (color, ms) in frames {
            let mut buf = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 0]));
            buf.put_pixel(0, 0, *color);
            let frame = Frame::from_parts(
                buf,
                0,
                0,
                image::Delay::from_numer_denom_ms(*ms, 1),
            );
            enc.encode_frame(frame).expect("测试自产 GIF 应能编码");
        }
        drop(enc);
        out
    }

    #[test]
    fn gif_anim_decodes_frames_preserves_delay_and_bgra() {
        // 帧0：(0,0) 红 delay 100ms；帧1：(0,0) 蓝 delay 200ms
        let bytes = encode_test_gif(&[
            (image::Rgba([255, 0, 0, 255]), 100),
            (image::Rgba([0, 0, 255, 255]), 200),
        ]);
        let (frames, w, h) = decode_generic(&bytes).expect("自产 GIF 应能解码全帧");
        assert_eq!((w, h), (4, 3), "逻辑屏尺寸");
        assert_eq!(frames.len(), 2, "应解出全部 2 帧");
        for f in &frames {
            assert_eq!(f.buffer().dimensions(), (4, 3), "每帧必须统一逻辑屏尺寸");
        }
        // delay 保留（10ms 精度下 100/200ms 精确往返）
        assert_eq!(
            std::time::Duration::from(frames[0].delay()),
            std::time::Duration::from_millis(100)
        );
        assert_eq!(
            std::time::Duration::from(frames[1].delay()),
            std::time::Duration::from_millis(200)
        );
        // BGRA 字节序：(0,0) 帧0 红 → [蓝0,绿0,红255,255]；帧1 蓝 → [蓝255,绿0,红0,255]
        let f0 = frames[0].buffer().as_raw();
        assert_eq!([f0[0], f0[1], f0[2], f0[3]], [0, 0, 255, 255], "帧0 红像素应为 BGRA");
        let f1 = frames[1].buffer().as_raw();
        assert_eq!([f1[0], f1[1], f1[2], f1[3]], [255, 0, 0, 255], "帧1 蓝像素应为 BGRA");
    }

    #[test]
    fn gif_single_frame_decodes_as_one_frame() {
        let bytes = encode_test_gif(&[(image::Rgba([10, 20, 30, 255]), 0)]);
        let (frames, w, h) = decode_generic(&bytes).expect("单帧 GIF 应能解码");
        assert_eq!((w, h), (4, 3));
        assert_eq!(frames.len(), 1, "单帧 GIF 只有一帧");
    }

    #[test]
    fn gif_bad_bytes_never_panic_and_report_or_fallback() {
        // GIF 魔数开头但内容损坏：整链路绝不 panic；落 `Err` 即算通过（坏文件回退由
        // 上层「首帧静态」兜底，这里只保证解码入口本身不炸）。
        let bytes = b"GIF89a this is definitely not a valid gif body.........";
        let ok = match decode_generic(bytes) {
            Ok((frames, _, _)) => !frames.is_empty(),
            Err(_) => true,
        };
        assert!(ok, "坏 GIF 不得 panic、不得产出空帧");
        // 完全非图片字节 → 明确报错
        assert!(decode_generic(b"not an image, just plain text").is_err());
    }

    #[test]
    fn gif_frame_cap_keeps_only_first_n_frames() {
        let mut frames = Vec::new();
        for i in 0..(MAX_GIF_FRAMES + 50) {
            frames.push((image::Rgba([(i % 256) as u8, 0, 0, 255]), 10));
        }
        let bytes = encode_test_gif(&frames);
        let (got, _, _) = decode_generic(&bytes).expect("超长 GIF 应能解码");
        assert_eq!(
            got.len(),
            MAX_GIF_FRAMES,
            "超 MAX_GIF_FRAMES 应截断到前 N 帧"
        );
    }

    /// 动图时钟回归：安装后由本查看器后台时钟逐帧推进（[`ImageViewer::spawn_anim_clock`]）。
    /// gpui `img()` 自动动画只在窗口聚焦时推进（失焦冻结）——本查看器改自驱时钟后不依赖
    /// 窗口焦点，时钟推进即证明失焦也会继续播放。用 `advance_clock` 模拟时间流逝断言帧序。
    #[gpui::test]
    async fn gif_clock_advances_frames_after_install(cx: &mut TestAppContext) {
        let dir = tmp_dir("gif_clock");
        let p = dir.join("anim.gif");
        let bytes = encode_test_gif(&[
            (image::Rgba([10, 20, 30, 255]), 120),
            (image::Rgba([200, 100, 50, 255]), 120),
        ]);
        std::fs::write(&p, &bytes).unwrap();
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        cx.update(|app| {
            let v = viewer.read(app);
            assert!(v.anim_clip.is_some(), "动图应挂帧切片");
            assert_eq!(v.anim_index, 0, "初始为第 0 帧");
        });
        // 越过第 0 帧时长（120ms）→ 自驱时钟应推进到第 1 帧（不依赖窗口焦点）
        flush(cx);
        cx.executor().advance_clock(std::time::Duration::from_millis(150));
        flush(cx);
        let idx = cx.update(|app| viewer.read(app).anim_index);
        assert_eq!(idx, 1, "自驱时钟应推进到第 1 帧（失焦也继续播放）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实 GIF 动图实测（`--ignored --nocapture` 运行；缺夹具 SKIP，同 pdf 夹具姿势）：
    /// 对 [`MAX_GIF_FRAMES`] 内的全帧解码，验证（1）每帧都是逻辑屏全画布
    /// （image 0.25 已按 disposal/偏移合成、统一尺寸）；（2）相邻采样帧字节内容确有差异
    /// （动画在动，非全同帧）。
    #[test]
    #[ignore]
    fn gif_real_animation_full_canvas() {
        let path = std::path::Path::new(r"C:\Users\yuexi\Pictures\test\咕嘎可爱日常.gif");
        if !path.exists() {
            eprintln!("SKIP: missing {}", path.display());
            return;
        }
        let bytes = std::fs::read(path).unwrap();
        let (frames, w, h) = decode_generic(&bytes).expect("真实 GIF 应能解码全帧");
        eprintln!(
            "\n[GIF-REAL] {} | 逻辑屏 {w}×{h} | 帧数 {}",
            path.display(),
            frames.len()
        );
        assert!(frames.len() > 1, "动图应 >1 帧");
        let mut any_differ = false;
        let step = (frames.len() / 8).max(1);
        for i in (0..frames.len()).step_by(step) {
            let (fw, fh) = frames[i].buffer().dimensions();
            assert_eq!((fw, fh), (w, h), "帧[{i}] 必须全画布（逻辑屏尺寸）");
            let j = (i + 1).min(frames.len() - 1);
            let a = frames[i].buffer().as_raw();
            let b = frames[j].buffer().as_raw();
            let diff = a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
            any_differ |= diff > 0;
            eprintln!(
                "  帧[{i:>4}] → 帧[{j:>4}] 差异字节 {diff:>8} · delay {}ms",
                frames[i].delay().numer_denom_ms().0
            );
        }
        assert!(any_differ, "采样邻帧字节应有差异（合成在动）");
        eprintln!("[GIF-REAL] 全画布 ✓ · 帧间确有内容差异 ✓（动画在动）");
    }

    /// 渲染路径冒烟（`--ignored --nocapture`；缺夹具 SKIP）：真实 GIF 走真实窗口
    /// `add_window_view + 绘制一帧`——img 主图元素带了动态 element id，走 gpui stateful
    /// 生命周期（global_id 判定 + 动画帧预检），必须不崩、窗口能完整画一帧。
    #[gpui::test]
    #[ignore]
    async fn gif_real_open_paints_first_frame(cx: &mut TestAppContext) {
        let path = std::path::Path::new(r"C:\Users\yuexi\Pictures\test\咕嘎可爱日常.gif");
        if !path.exists() {
            eprintln!("SKIP: missing {}", path.display());
            return;
        }
        let (viewer, wcx) =
            cx.add_window_view(|_window, cx| ImageViewer::new(path.to_path_buf(), cx));
        let mut guard = 0;
        loop {
            wcx.run_until_parked();
            if wcx.update(|_, cx| viewer.read(cx).is_loaded()) {
                break;
            }
            guard += 1;
            assert!(guard < 2000, "等待 GIF 解码就绪超时");
        }
        // 绘制一帧（标记脏 + 排空执行器走到绘制）：带 id 的 img 元素不得崩
        let _ = wcx.refresh();
        wcx.run_until_parked();
        eprintln!("[GIF-REAL] 渲染冒烟 ✓（解码就绪 + 绘制一帧完成）");
    }

    /// 写一张有内容（渐变）的 JPEG：保存走 `image` 内置 jpeg 编码（default features 支持）。
    fn write_jpeg(path: &Path, w: u32, h: u32) {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            let b = ((x + y) * 60 / (w + h)) as u8;
            *px = image::Rgb([r, g, b]);
        }
        img.save(path).expect("写出渐变 JPEG 应成功");
    }

    /// 组合「带 EXIF 嵌入缩略图」的合法 JPEG 字节（SOI + APP1-Exif（TIFF 内嵌缩略 JPEG），
    /// 再接主图去 SOI 的其余段）。II 小端 TIFF，0x0201/0x0202 指向内嵌缩略 JPEG——免依赖地
    /// 造出真实手机照片的 EXIF 结构（school 已实测确实带缩略图）。
    fn wrap_exif_thumb(main: &[u8], thumb: &[u8]) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @ 8
        tiff.extend_from_slice(&0u16.to_le_bytes()); // IFD0 0 项 (8..10)
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next 占位 (10..14)
        let ifd1 = tiff.len() as u32; // = 14
        tiff[10..14].copy_from_slice(&ifd1.to_le_bytes()); // 回填 next → IFD1 @ 14
        tiff.extend_from_slice(&2u16.to_le_bytes()); // IFD1 2 项 (14..16)
        tiff.extend_from_slice(&0x0201u16.to_le_bytes()); // tag
        tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count=1
        let off_pos = tiff.len();
        tiff.extend_from_slice(&0u32.to_le_bytes()); // 占位：缩略图偏移
        tiff.extend_from_slice(&0x0202u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        let len_pos = tiff.len();
        tiff.extend_from_slice(&0u32.to_le_bytes()); // 占位：缩略图长度
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
        let thumb_off = tiff.len() as u32;
        tiff[off_pos..off_pos + 4].copy_from_slice(&thumb_off.to_le_bytes());
        tiff[len_pos..len_pos + 4].copy_from_slice(&(thumb.len() as u32).to_le_bytes());
        tiff.extend_from_slice(thumb);

        let mut payload = Vec::new();
        payload.extend_from_slice(b"Exif\0\0");
        payload.extend_from_slice(&tiff);
        let mut out = Vec::new();
        out.extend_from_slice(&[0xFF, 0xD8]); // SOI（恰 2 字节）
        out.extend_from_slice(&[0xFF, 0xE1]); // APP1
        let seg_len = (payload.len() + 2) as u16;
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&payload);
        out.extend_from_slice(&main[2..]); // 主图（去掉 2 字节 SOI）
        out
    }

    #[test]
    fn extract_exif_jpeg_thumbnail_reads_embedded_thumb() {
        let dir = tmp_dir("exif");
        write_jpeg(&dir.join("thumb.jpg"), 160, 120);
        write_jpeg(&dir.join("main.jpg"), 800, 600);
        let thumb = std::fs::read(dir.join("thumb.jpg")).unwrap();
        let main = std::fs::read(dir.join("main.jpg")).unwrap();
        let full = wrap_exif_thumb(&main, &thumb);
        let got = extract_exif_jpeg_thumbnail(&full).expect("应提取到 EXIF 内嵌缩略图");
        assert_eq!(got, thumb, "提取的应与写入的缩略 JPEG 完全一致");
        // 提取结果能按 1/1 全解成 160×120 BGRA
        let (bgra, w, h, den) = decode_jpeg_display(&got, DISPLAY_NEED_INIT).unwrap();
        assert_eq!((w, h, den), (160, 120, 1));
        assert_eq!(bgra.len(), (160 * 120 * 4) as usize);
        // 加了 EXIF 段后主图仍是合法 JPEG：按展示需要 (400,300) → 1/2 档
        let (_, w2, h2, d2) = decode_jpeg_display(&full, (400.0, 300.0)).unwrap();
        assert_eq!((w2, h2, d2), (400, 300, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_exif_jpeg_thumbnail_none_on_plain_jpeg() {
        let dir = tmp_dir("exif_none");
        write_jpeg(&dir.join("plain.jpg"), 64, 64);
        let bytes = std::fs::read(dir.join("plain.jpg")).unwrap();
        assert!(extract_exif_jpeg_thumbnail(&bytes).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clamp_pan_covers_viewport_and_stays_centered_when_unzoomed() {
        let cp = ImageViewer::clamp_pan;
        // 未放大（图适合视口）：任一轴 pan 都 clamp 到 0 → 保持居中，拖拽无效果
        assert_eq!(cp((50.0, -30.0), 1000.0, 700.0, 500.0, 300.0), (0.0, 0.0));
        // 横向放大（图 1500 > 视口 1000）：可滑范围 ±250，越界被裁剪；纵向仍适配 → y=0
        assert_eq!(cp((1000.0, 999.0), 1000.0, 700.0, 1500.0, 700.0), (250.0, 0.0));
        assert_eq!(cp((-1000.0, 0.0), 1000.0, 700.0, 1500.0, 700.0), (-250.0, 0.0));
        // 两轴都放大：各轴独立 clamp 到 ±(图-视口)/2
        assert_eq!(cp((-1000.0, 200.0), 1000.0, 700.0, 1500.0, 900.0), (-250.0, 100.0));
        // 范围内不动
        assert_eq!(cp((80.0, -40.0), 1000.0, 700.0, 1500.0, 900.0), (80.0, -40.0));
    }

    /// Ctrl+滚轮缩放边界（走 [`Self::adjust_zoom`]）：一档 = `1 ± ZOOM_STEP`，逐格放大/
    /// 缩小分别钳到 [`ZOOM_MIN`] / [`ZOOM_MAX`]。缩放副作用 [`Self::escalate_display_denom`]
    /// 在 `display_denom==1` 时直接返回、不派发解码，故可直接单测。
    #[gpui::test]
    async fn adjust_zoom_clamps_to_minmax_and_steps(cx: &mut TestAppContext) {
        let dir = tmp_dir("zoom_clamp");
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.intrinsic = Some((200.0, 100.0));
                v.last_viewport = (800.0, 600.0);
                v
            })
        });
        // 一档 = 1 + ZOOM_STEP（走共享 controller 的档位生成）；再缩一档 = 1×0.88 =
        // (1+s)(1-s)=1-s² ≈ 0.9856（非回到 1.0——原行为即如此，逐格独立算因子）。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                assert!((v.zoom - 1.12).abs() < 1e-6, "1 档放大: {}", v.zoom);
                v.adjust_zoom(pan_zoom::zoom_factor(-1.0, ZOOM_STEP), cx);
                assert!((v.zoom - 0.9856).abs() < 1e-4, "再缩一档: {}", v.zoom);
            })
        });
        // 缩到下限 / 放大到上限
        for _ in 0..200 {
            cx.update(|app| {
                viewer.update(app, |v, cx| {
                    v.adjust_zoom(pan_zoom::zoom_factor(-1.0, ZOOM_STEP), cx);
                })
            });
        }
        assert_eq!(
            cx.update(|app| viewer.read(app).zoom),
            ZOOM_MIN,
            "向下滚到底应钳到下限"
        );
        for _ in 0..200 {
            cx.update(|app| {
                viewer.update(app, |v, cx| {
                    v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                })
            });
        }
        assert_eq!(
            cx.update(|app| viewer.read(app).zoom),
            ZOOM_MAX,
            "向上滚到底应钳到上限"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 放大态滚轮语义：先在图片内纵向平移，滚到边缘且继续同向外滚才切下一张；切图时
    /// 缩放复位为适应视口（1.0）、平移归零。（直接构造状态，绕过真实解码/布局。）
    #[gpui::test]
    async fn wheel_zoomed_pans_within_then_switches_at_edge(cx: &mut TestAppContext) {
        let dir = tmp_dir("wheel_pan");
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.nav = FolderNav::new(
                    vec![dir.join("a.jpg"), dir.join("b.jpg"), dir.join("c.jpg")],
                    0,
                );
                v.last_viewport = (800.0, 600.0);
                v.intrinsic = Some((200.0, 100.0));
                v.zoom = 4.0; // 放大后 3008×1504 远超视口
                v
            })
        });
        // 放大态：向下滚 100px → 只平移（看下方），不切图
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.wheel_pan_or_switch(1, 100.0, cx);
                assert_eq!(v.pan, (0.0, -100.0), "放大态应先纵向平移而非切图");
                assert_eq!(v.nav.current(), 0);
            })
        });
        // 一路滚到纵向边缘（贴边 contain：缩放 4× → 3200×1600，ay=(1600-600)/2=500）再继续
        // 同向滚 → 才切下一张，且缩放复位
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.pan.1 = -500.0; // 已到边缘
                v.wheel_pan_or_switch(1, 100.0, cx);
                assert_eq!(v.nav.current(), 1, "滚到边缘继续同向外滚才切下一张");
                assert_eq!(v.zoom, 1.0, "切图后缩放复位为适应视口");
                assert_eq!(v.pan, (0.0, 0.0), "切图后平移归零");
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未放大滚轮语义：保持直接切图（不进入平移）。
    #[gpui::test]
    async fn wheel_unzoomed_switches_immediately(cx: &mut TestAppContext) {
        let dir = tmp_dir("wheel_sw");
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.nav = FolderNav::new(
                    vec![dir.join("a.jpg"), dir.join("b.jpg"), dir.join("c.jpg")],
                    0,
                );
                v.last_viewport = (800.0, 600.0);
                v.intrinsic = Some((100.0, 50.0)); // 1× 适配视口（752×376 < 800×600）
                v.zoom = 1.0;
                v
            })
        });
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.wheel_pan_or_switch(1, 50.0, cx);
                assert_eq!(v.nav.current(), 1, "未放大：滚轮仍直接切下一张");
                assert_eq!(v.zoom, 1.0);
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁定态切换：`toggle_lock` 翻转 `is_locked` 并把放大后的平移/拖拽归零（干净看图）。
    #[gpui::test]
    async fn toggle_lock_flips_state_and_resets_pan(cx: &mut TestAppContext) {
        let dir = tmp_dir("lock");
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.zoom = 4.0;
                v.pan = (100.0, -50.0); // 放大平移中
                v
            })
        });
        assert!(!cx.update(|app| viewer.read(app).is_locked()));
        cx.update(|app| viewer.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(cx.update(|app| viewer.read(app).is_locked()));
        assert_eq!(cx.update(|app| viewer.read(app).pan), (0.0, 0.0), "锁定应归零平移");
        cx.update(|app| viewer.update(app, |v, cx| v.toggle_lock(cx)));
        assert!(!cx.update(|app| viewer.read(app).is_locked()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁定模式信息条时序：切图浮现 → 1s 自隐；悬停热区浮现且不隐藏、移开 1s 自隐。
    #[gpui::test]
    async fn locked_info_bar_shows_on_switch_hover_and_auto_hides(cx: &mut TestAppContext) {
        let dir = tmp_dir("lockinfo");
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.nav = FolderNav::new(vec![dir.join("a.jpg"), dir.join("b.jpg")], 0);
                v.locked = true;
                v
            })
        });
        // 初始：锁定未悬停 → 隐藏；切图 → 浮现
        assert!(!cx.update(|app| viewer.read(app).info_auto.visible()));
        cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        assert!(
            cx.update(|app| viewer.read(app).info_auto.visible()),
            "锁定模式切图应立即浮现信息条"
        );
        // 1s 后自隐
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| viewer.read(app).info_auto.visible()),
            "切图后 1s 应自隐"
        );
        // 悬停热区 → 浮现，且过了 2s 也不隐藏
        cx.update(|app| viewer.update(app, |v, cx| v.set_info_hover(true, cx)));
        assert!(cx.update(|app| viewer.read(app).info_auto.visible()));
        cx.executor().advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        assert!(cx.update(|app| viewer.read(app).info_auto.visible()), "悬停期间不隐藏");
        // 移开 → 短暂保留，1s 后自隐
        cx.update(|app| viewer.update(app, |v, cx| v.set_info_hover(false, cx)));
        assert!(cx.update(|app| viewer.read(app).info_auto.visible()), "移开后仍短暂保留");
        cx.executor().advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        assert!(
            !cx.update(|app| viewer.read(app).info_auto.visible()),
            "移开 1s 后自隐"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jpeg_scaled_decode_bgra_direct_and_sharp_boundary() {
        let dir = tmp_dir("jpeg_disp");
        let p = dir.join("big.jpg");
        write_jpeg(&p, 400, 300);
        let bytes = std::fs::read(&p).unwrap();
        // 展示需要 (200,150) → 1/2 档 200×150；BGRA 直出（JPEG 恒单帧）
        let (frames, w, h, den) = decode_background(&bytes, (200.0, 150.0)).expect("JPEG 降档应成功");
        assert_eq!((w, h, den), (200, 150, 2));
        assert_eq!(frames.len(), 1, "JPEG 应为单帧");
        let bgra = frames[0].buffer().as_raw();
        assert_eq!(bgra.len(), (200 * 150 * 4) as usize);
        // 左上角 (0,0) 黑色 → BGRA [0,0,0,255]；右下角 (399,299) 渐变近
        // B≈60,G≈254,R≈254 → BGRA [B,G,R,255]（libjpeg-turbo Bgra 直出 = GPUI 字节序）。
        assert_eq!([bgra[0], bgra[1], bgra[2], bgra[3]], [0, 0, 0, 255]);
        let tail = &bgra[(bgra.len() - 4)..];
        assert!(tail[0] < 150 && tail[1] > 200 && tail[2] > 200, "右下应低蓝高红：{tail:?}");
        // 展示需要 ≥ 原图 → 全解（1/1）
        let (_b, w2, _h2, den2) = decode_background(&bytes, (400.0, 400.0)).unwrap();
        assert_eq!((w2, den2), (400, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_jpeg_denom_never_below_display_pixels() {
        // 20MP + 1080p 视口需要 (1376,1032)：1/2（2560×1920）恰好全cover、1/4（1280×960）
        // 会低于 1376 → 必须选 1/2 —— 这就是「不糊」的边界。
        assert_eq!(pick_jpeg_denom(5120, 3840, (1376.0, 1032.0)), 2);
        assert_eq!(pick_jpeg_denom(5120, 3840, (1280.0, 960.0)), 4);
        assert_eq!(pick_jpeg_denom(5120, 3840, (1281.0, 960.0)), 2, "超一像素也必须升 1/2");
        // 缩小看（zoom<1）→ 需要变小 → 可压到 1/8（640×480 ≥ 275×206）
        assert_eq!(pick_jpeg_denom(5120, 3840, (275.0, 206.0)), 8);
        // 放大 2× → 需要 (2752,2064) > 1/2（2560×1920）→ 全解
        assert_eq!(pick_jpeg_denom(5120, 3840, (2752.0, 2064.0)), 1);
        // 小图不需要降档
        assert_eq!(pick_jpeg_denom(800, 600, (1920.0, 1080.0)), 1);
    }

    /// 端到端：打开 20MP JPEG → 首解即「显示尺寸档」（1/2，不糊），而非低清 1/8；缓存也只
    /// 存这份清晰档。测试执行器无窗口，用 [`DISPLAY_NEED_INIT`]（≈1080p）作为展示需要。
    #[gpui::test]
    async fn jpeg_cold_open_installs_sharp_display(cx: &mut TestAppContext) {
        let dir = tmp_dir("sharp");
        let p = dir.join("big.jpg");
        write_jpeg(&p, 5120, 3840);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        let (den, w, h, cache_len) = cx.update(|app| {
            let v = viewer.read(app);
            let c = v.cache.len();
            (
                v.display_denom,
                v.intrinsic.map(|i| i.0 as u32).unwrap_or(0),
                v.intrinsic.map(|i| i.1 as u32).unwrap_or(0),
                c,
            )
        });
        // 展示需要下限 = DISPLAY_NEED_INIT(1920×1080) → 1/2 档（2560×1920），分辨率 ≥ 显示，不糊
        assert_eq!(den, 2, "首解应为 1/2 显示尺寸档，而非低清 1/8：{den}");
        assert_eq!((w, h), (2560, 1920));
        assert_eq!(cache_len, 1, "缓存应是唯一一份清晰档");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1b 端到端：带 EXIF 嵌入缩略图的 JPEG 冷打开 → 立即有占位首帧（缩略图 160×120，
    /// `loading` 保持 true，不升档、不进 LRU），随后展示档真图覆盖为清晰档（800×600）。
    /// 验证「即时首帧」接线不破坏既有的落盘守卫与「首解即清晰档」路径。
    #[gpui::test]
    async fn jpeg_cold_open_exif_thumb_then_sharp(cx: &mut TestAppContext) {
        let dir = tmp_dir("exif_e2e");
        // 缩略图写进子目录：避免成为 big.jpg 同目录清单的第二个图片（否则会被级联预取、
        // 污染 cache_len 断言）
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        write_jpeg(&assets.join("thumb.jpg"), 160, 120);
        write_jpeg(&dir.join("big.jpg"), 800, 600);
        let thumb = std::fs::read(assets.join("thumb.jpg")).unwrap();
        let main = std::fs::read(dir.join("big.jpg")).unwrap();
        std::fs::write(
            dir.join("big.jpg"),
            wrap_exif_thumb(main.as_slice(), thumb.as_slice()),
        )
        .unwrap();
        let p = dir.join("big.jpg");
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        let (w, h, den, loaded, cache_len, pending) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.intrinsic.map(|i| i.0 as u32).unwrap_or(0),
                v.intrinsic.map(|i| i.1 as u32).unwrap_or(0),
                v.display_denom,
                !v.loading,
                v.cache.len(),
                v.thumb_pending.is_none(),
            )
        });
        // 真图就绪后必须覆盖为清晰档；缩略图占位不留在 LRU 缓存里
        assert_eq!((w, h), (800, 600), "EXIF 缩略图只是占位，最终应落 800×600 清晰档");
        assert_eq!(den, 1, "800×600 < 展示需要 → 全解档");
        assert!(loaded, "真图落盘后 loading 应为 false");
        assert_eq!(cache_len, 1, "LRU 缓存只应有唯一一份真图，缩略图占位不入缓存");
        assert!(pending, "缩略图任务完成后 thumb_pending 应清空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 升档（评审补测，CRG 风险 0.71）：放大使展示像素需要超过 1/2 档时，按需重解 1/1 全解
    /// 替换（1:1 看细节仍清晰），且只升不降、不再重复触发（去重）。
    #[gpui::test]
    async fn zoom_in_escalates_display_to_full_resolution(cx: &mut TestAppContext) {
        let dir = tmp_dir("escalate");
        let p = dir.join("big.jpg");
        write_jpeg(&p, 5120, 3840);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        assert_eq!(cx.update(|app| viewer.read(app).display_denom), 2, "首解 1/2");
        // 模拟放大 ≥2×：展示像素需要 (2752,2064) > 1/2(2560×1920) → 应升到 1/1 全解
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.display_need = (2752.0, 2064.0);
                v.escalate_display_denom(cx);
            })
        });
        flush(cx);
        let (d1, w1, h1, cached) = cx.update(|app| {
            let v = viewer.read(app);
            let c: Vec<(u8, u32, u32)> = v
                .cache
                .iter()
                .map(|e| (e.denom, e.intrinsic.0 as u32, e.intrinsic.1 as u32))
                .collect();
            (
                v.display_denom,
                v.intrinsic.map(|i| i.0 as u32).unwrap_or(0),
                v.intrinsic.map(|i| i.1 as u32).unwrap_or(0),
                c,
            )
        });
        assert_eq!(d1, 1, "放大 2× 后应升档到 1/1 全解，而非停在 1/2");
        assert_eq!((w1, h1), (5120, 3840));
        assert_eq!(cached, vec![(1, 5120, 3840)], "缓存应升级为唯一全解档");
        // 再次触发（无需升档）应为空操作：不新增解码、档位不变
        cx.update(|app| viewer.update(app, |v, cx| v.escalate_display_denom(cx)));
        let stable = cx.update(|app| viewer.read(app).display_denom);
        assert_eq!(stable, 1);
        let _ = std::fs::remove_dir_all(&dir);
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
        let nav = FolderNav::scan(&current, crate::is_image_extension);
        let names: Vec<String> = nav
            .items()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // md 排除；WEbP 大小写不敏感识别；清单按文件名升序。
        assert_eq!(names, vec!["a.png", "b.png", "c.jpeg", "logo.WEBP"]);
        assert_eq!(nav.current(), 1);
        assert_eq!(nav.current_path(), current);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_folder_empty_dir_falls_back_to_single_item() {
        let p = tmp_dir("orf").join("orphan.png");
        let nav = FolderNav::scan(&p, crate::is_image_extension);
        assert_eq!(nav.len(), 1);
        assert_eq!(nav.current(), 0);
        assert_eq!(nav.current_path(), p);
    }

    /// 实测大图冷解码耗时：**全分辨率（旧链路 `image` 全解）vs 展示尺寸档（新链路
    /// libjpeg-turbo SIMD 降档直出 BGRA）**。展示需要固定为 1080p 视口 contain 的真实量
    /// （1376×1032）——验证 20MP 照片打开/切换由 debug ~7s 降到几十 ms、且解码分辨率 ≥ 显示
    /// （不糊）。目录 `IMG_BENCH_DIR`（school 用 school），`IMG_BENCH_LIMIT` 前 N 张。
    /// `--ignored --nocapture` 运行。
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
            .filter(|p| crate::is_image_extension(p))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "目录内无图片可测：{dir}");
        // 可选上限（`IMG_BENCH_LIMIT`），测大文件夹时只跑前几张即可看数量级
        if let Ok(lim) = std::env::var("IMG_BENCH_LIMIT")
            && let Ok(n) = lim.parse::<usize>()
        {
            files.truncate(n);
        }
        // 1080p 视口 contain 需要的展示像素（≈ 结果「显示尺寸档」的选档基准）
        let need = (1376.0, 1032.0);
        eprintln!(
            "\n[IMG-BENCH] {} 张 | 全解(旧 image) vs 展示尺寸(libjpeg-turbo SIMD 降档, 需要 {:.0}×{:.0})：",
            files.len(),
            need.0,
            need.1
        );
        let mut full: Vec<f64> = Vec::new();
        let mut disp: Vec<f64> = Vec::new();
        for p in &files {
            let meta = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            let t0 = std::time::Instant::now();
            let full_res = decode_image_at(p);
            let ms_full = t0.elapsed().as_secs_f64() * 1000.0;
            let t1 = std::time::Instant::now();
            let disp_res = decode_display_at(p, need);
            let ms_disp = t1.elapsed().as_secs_f64() * 1000.0;
            match (full_res, disp_res) {
                (Ok((_a, wf, hf)), Ok((_b, wd, hd, den))) => {
                    eprintln!(
                        "  旧 {:7.1}ms {:>9}px | 新 {:7.1}ms {:>9}px (1/{})  提速 {:6.1}×   {:6.1} KB  {}",
                        ms_full,
                        wf as usize * hf as usize,
                        ms_disp,
                        wd as usize * hd as usize,
                        den,
                        ms_full / ms_disp.max(0.001),
                        meta as f64 / 1024.0,
                        p.file_name().unwrap().to_string_lossy()
                    );
                    full.push(ms_full);
                    disp.push(ms_disp);
                }
                (Err(e), _) => eprintln!("  ERR（全解）{}: {e}", p.display()),
                (_, Err(e)) => eprintln!("  ERR（展示）{}: {e}", p.display()),
            }
        }
        if !full.is_empty() && !disp.is_empty() {
            let avg = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
            let mmin = |v: &Vec<f64>| v.iter().cloned().fold(f64::MAX, f64::min);
            let mmax = |v: &Vec<f64>| v.iter().cloned().fold(f64::MIN, f64::max);
            let f_avg = avg(&full);
            let d_avg = avg(&disp);
            eprintln!(
                "  --- 全解(旧)：均值 {:.1}ms / min {:.1}ms / max {:.1}ms",
                f_avg,
                mmin(&full),
                mmax(&full)
            );
            eprintln!(
                "  --- 展示尺寸(新)：均值 {:.1}ms / min {:.1}ms / max {:.1}ms （综合提速 {:.1}×）",
                d_avg,
                mmin(&disp),
                mmax(&disp),
                f_avg / d_avg.max(0.001)
            );
            eprintln!(
                "  --- 切换体感：缓存命中 ≈ 0ms；冷目标按展示档解码 ≈ {:.0}ms，随后预取全命中",
                d_avg
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
            (v.cache.len(), v.nav.len())
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
            config::image::PREFETCH_FORWARD,
            config::image::PREFETCH_BACKWARD
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
