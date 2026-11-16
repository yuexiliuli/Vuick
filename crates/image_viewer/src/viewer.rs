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
//!   又不会每一滚都从头解码；**快滚节流**：连续快速滚动（`scroll_fast`）时有效并发收敛到
//!   [`FAST_SCROLL_MAX_DECODE`]=2，避免 8 路并发把核打满（CPU 降、追尾更短）；
//! - **磁盘→RAM 字节预读**：文件**压缩字节** `fs::read` 进 LRU 字节缓存，与解码分离——
//!   解码从内存字节开始，不再等磁盘 IO；重解码/回看不重读盘；
//! - **解码结果 LRU 缓存**（张数 + 字节双上限）：命中即 O(1) 瞬时上屏。
//!
//! # 大图打开/滚动（school 20MP 照片，CPU/内存双降 + 清晰）
//! - **按展示像素解码（decode to display size）**：JPEG 走自研 `kimg` 全量解出 RGBA8 后，
//!   与 PNG 同款**盒式降采样**到展示档（无 `DynamicImage` 中间拷贝之外的额外开销），
//!   按 [`Self::display_need`] 档位解码。选档规则 = **解码分辨率 ≥ 显示需要的像素**
//!   （上限 1/8，需要涨则分母落，需要超过原图才全解）——GPU 只缩小不放大 → **显示不糊**；
//!   单帧内存 75MB→~20MB。
//! - **PNG 同款降档**：PNG 走 `kimg::codecs::png::PngDecoder` 全量解出 RGBA8（palette/
//!   灰度/16bit 展开归一）后按同一选档规则**盒式降采样**到展示档（rayon 行组并行，小图
//!   退串行）——缓存/GPU 上传像素数按 den² 缩小，放大升档链路与 JPEG 对齐。
//! - **内存/CPU 有界**：解码并发 `[Self::max_decode]`-1 预取封顶（快速滚动再收窄到
//!   [`FAST_SCROLL_MAX_DECODE`]），且 [`Self::pump_prefetch`]
//!   在解码缓存达到字节预算后**停止派发**（[`CACHE_MAX_BYTES`]）——不再把整个文件夹扫一遍
//!   白算 CPU（此前 106 张全解 ≈ 8GB 像素量磨 8 核）。缓存能装 ~9 张显示档 → 滚动基本全命中。
//! - **放大升档**：Ctrl+滚轮使 `display_need` 变大 → `escalate_display_denom` 按需
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

use gpui::{App, Context, Hsla, RenderImage, ScrollHandle, Window};
use kimg::{AnimationDecoder, Frame, ImageDecoder, RgbaImage};

use config::image::{BYTE_CACHE_BYTES, CACHE_MAX_BYTES, CACHE_MAX_ITEMS};
// 以下常量仅 `mod tests`（`use super::*`）以裸名直取；非测试构建不导入，避免未用告警。
// （PREFETCH_FORWARD/PREFETCH_BACKWARD 测试里全限 `config::image::`，无需在此导入。）
#[cfg(test)]
use config::image::{PREFETCH_FORWARD_FAST, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP};
/// 展示像素需要初始值：窗口/视口数据未知（首个 render 前）时的合理猜测 ≈1080p，
/// 保证首解就是「显示尺寸档」而非低清档；首个 render 立即用真实视口校正。
const DISPLAY_NEED_INIT: (f32, f32) = (1920.0, 1080.0);
/// GIF 动图最多保留的帧数：超出只取前 N 帧（动画仍循环），资源护栏防超大 GIF 用帧数
/// 把内存/上传预算打爆（超大文件另有 LRU 字节预算如实兜底）。
const MAX_GIF_FRAMES: usize = 256;

use ::viewer::auto_hide;
use ::viewer::folder::FolderNav;
use ::viewer::pan_zoom;

// 子模块（按职责拆分；本文件保留「构造/状态/几何骨架 + 低层解码原语 + 测试块」）。
// 私有成员经 `super::` 可见，除 `pub(super)` 最小修正外不改可见性。
mod input;   // input/hit：滚轮缩放/切图、放大态平移、拖拽平移、切图复位
mod decode;  // decode：并行解码池 / LRU 缓存 / 级联预取 / 动图时钟 / EXIF 即时首帧
mod display; // display/placeholder：Render 上屏、C6 预上传垫底、占位文本
mod diag;    // diag：env 门控诊断探针（VUICK_IMG_DIAG=1，帧间隔/切换时延）

/// 一张已解码可立即上屏的图片（LRU 缓存条目）。
#[derive(Clone)]
struct CacheEntry {
    path: PathBuf,
    render: Arc<RenderImage>,
    intrinsic: (f32, f32),
    bytes: usize,
    /// 实际解码降档分母（1/8/4/2/1）：JPEG/PNG/HEIF/通用静态共用 [`pick_den`] 选档
    /// （JPEG 全解后
    /// 盒式降采样、PNG 盒式降采样），其它格式恒 1。用于「升档判定」：展示分母 >
    /// 当前需要分母（= 解码比显示更粗）时按需重解更细档（放大时）。
    denom: u8,
    /// 动图（GIF）逐帧切片：每帧一个**独立单帧** `RenderImage` + 各自时长；`None` = 静态图。
    /// 采用「按帧切片」而非 gpui 多帧 `RenderImage`：gpui `img()` 元素的自动动画只在窗口
    /// 聚焦（`window.is_window_active()`）时推进，失焦即冻结——改为本查看器用后台时钟
    /// 逐帧推进 `self.state.data`（单帧 RenderImage），每次渲染直接画当前帧，与窗口焦点无关。
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
    /// 异步加载状态（loading / load_error / 已解码图片）：`data` = 已解码图片（`Some` =
    /// 可上屏；切换加载期间保留旧图避免闪白）；`load_error` 渲染时优先于 `data`（有错误
    /// 显示错误占位）。
    state: ui::AsyncState<Arc<RenderImage>>,
    /// 图片固有像素尺寸（显示比例依据）。
    intrinsic: Option<(f32, f32)>,
    /// 同文件夹兄弟文件导航（清单文件名升序、恒含当前图、恒非空；循环翻页语义收敛在
    /// [`FolderNav`]）。
    nav: FolderNav,
    /// 缩放倍数（1.0 = 适应视口；Ctrl+滚轮调）。
    zoom: f32,
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
    /// **重型格式**（HEIF/HEIC）在途解码数：受 [`config::image::HEAVY_DECODE_MAX`] 单独封顶
    /// ——HEIF 全解秒级 + 数十 MB RGBA，8 路并发会把核打满致 GUI 卡死；JPEG/PNG/GIF 不计数。
    heavy_inflight: usize,
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
    /// `[Self::max_decode]`-1（快速滚动节流到 [`config::image::FAST_SCROLL_MAX_DECODE`]-1），
    /// 保证冷目标能立刻占到一个槽、不用排在预取队尾等。
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
    /// 展示像素需要（宽, 高）：由「适应视口尺寸 × zoom」算出，**JPEG/PNG 解码档位的下限**——
    /// 解码分辨率 ≥ 该值（GPU 只缩小、不放大）→ 显示不糊。首个 render 前用
    /// [`DISPLAY_NEED_INIT`] 兜底。
    display_need: (f32, f32),
    /// 当前安装展示的降档分母（1/8/4/2/1；JPEG/PNG；其它格式恒 1）。缩放放大且需要更细档时
    /// 据此触发「升档」重解（`escalate_display_denom`）。
    display_denom: u8,
    /// 平移（px）：放大后拖拽浏览其他区域，以视口中心为基准、每帧 clamp 到图始终覆盖视口。
    pan: (f32, f32),
    /// 鼠标左键拖拽平移手势态（down/move/up 三方法收敛于 [`pan_zoom::PanGesture`]）。
    drag: pan_zoom::PanGesture,
    /// 「锁定」干净看图模式：隐藏全部非图片 UI、图片充满整个窗口（宿主读取决定外壳取舍）。
    locked: bool,
    /// 底部信息条浮层时序：可见位 + 自隐代序号 + 悬停态 + 延时（收敛于
    /// [`::viewer::auto_hide`]；切图/悬停浮现、1s 自隐、hover 抑制）。
    info_auto: auto_hide::AutoHide,
    /// 当前动图片段（帧切片 + 各帧时长）；`None` = 当前非动图。`install_current` 挂上，
    /// 由 `spawn_anim_clock` 的后台时钟逐帧推进 `self.state.data`。
    anim_clip: Option<(Vec<Arc<RenderImage>>, Vec<std::time::Duration>)>,
    /// 动图时钟代序号：每次 `install_current`（换图/重装）递增，旧时钟任务据此自停，
    /// 防止旧任务误推进新图帧。
    anim_gen: u64,
    /// 当前动图帧下标（0 = 首帧；配合 `self.state.data` 落盘当前帧切片）。
    anim_index: usize,
    /// 测试探针：`drop_image` 下线渲染图累计计数（仅 test 编译）。headless 无真实图集、
    /// `drop_image` 对零窗口为空操作、无法直接观测「GPU 释放本身」——以计数代理「效果断言」
    /// （K37），供 P1-29 回归断言「显示替换/翻页时旧渲染被 retire 下线」。
    #[cfg(test)]
    test_render_drops: usize,
}

impl ImageViewer {
    /// 创建查看器并异步加载 `path`（同文件夹扫描在后台；首图经缓存/worker 流程加载）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let this = Self {
            path: path.clone(),
            intrinsic: None,
            nav: FolderNav::new(vec![path.clone()], 0),
            zoom: 1.0,
            state: ui::AsyncState::loading(),
            viewport: ScrollHandle::new(),
            bg: None,
            cache: LruMap::new(Some(CACHE_MAX_ITEMS), CACHE_MAX_BYTES, |e: &CacheEntry| e.bytes),
            byte_cache: LruMap::new(None, BYTE_CACHE_BYTES, |e: &ByteEntry| e.bytes.len()),
            in_flight: HashSet::new(),
            byte_flight: HashSet::new(),
            heavy_inflight: 0,
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
            drag: pan_zoom::PanGesture::default(),
            locked: false,
            info_auto: auto_hide::AutoHide::new(),
            anim_clip: None,
            anim_gen: 0,
            anim_index: 0,
            #[cfg(test)]
            test_render_drops: 0,
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

    /// 重命名同步：当前显示文件被外部改名后更新内部 `path` + 同文件夹导航清单里同路径
    /// 条目（当前项下标不变、仅路径改名，前/后键与预取不再指到已不存在的旧名）。
    /// 文件内容未变，已解码渲染/缩放/平移保持不变。
    pub fn set_path(&mut self, path: PathBuf) {
        let old = std::mem::replace(&mut self.path, path.clone());
        let current = self.nav.current();
        let items: Vec<PathBuf> = self
            .nav
            .items()
            .iter()
            .map(|p| if *p == old { path.clone() } else { p.clone() })
            .collect();
        self.nav = FolderNav::new(items, current);
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
        self.state.is_loaded()
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
        self.drag = pan_zoom::PanGesture::default();
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
        if let Some(img) = self.state.data.take() {
            cx.drop_image(img, Some(window));
        }
        for entry in std::mem::take(&mut self.cache).into_iter() {
            // 静态图释放首帧 `render`；动图释放**全部帧切片**（对齐 LRU 逐出的
            // `drop_evicted_images` 口径——P1-30：旧实现只 drop 首帧 `render`，GIF 其余帧
            // 切片在此漏放，成为图集孤儿）。
            match entry.gif {
                Some((frames, _)) => {
                    for f in frames {
                        cx.drop_image(f, Some(window));
                    }
                }
                None => cx.drop_image(entry.render, Some(window)),
            }
        }
        // P1-30：动图时钟自停 + 释放动图片段引用。`anim_clip` 是帧切片的独立持有者，
        // 清缓存不覆盖它；且不递增 `anim_gen` 的话，release 后旧动画任务下个 tick 仍会把
        // 帧重新写回 `state.data`（空翻页/重挂 GPU 帧、纹理不释放）。先代序号自停再清引用。
        self.anim_gen = self.anim_gen.wrapping_add(1);
        self.anim_clip = None;
        self.anim_index = 0;
        self.byte_cache.clear();
        self.in_flight.clear();
        self.byte_flight.clear();
        self.heavy_inflight = 0;
        self.pending_target = None;
        self.thumb_pending = None;
        self.prefetch_inflight = 0;
        self.display_denom = 1;
        self.display_need = DISPLAY_NEED_INIT;
        self.pan = (0.0, 0.0);
        self.drag = pan_zoom::PanGesture::default();
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
        ui::theme::window_bg(cx)
    }
}

/// JPEG 魔数（FF D8 FF）检测：区分「JPEG 走展示档降档解码」与「其它格式走 image 全解」。
/// 以字节为准，比扩展名可信（伪装成 .jpg 的其它格式不会被误当 JPEG 硬解）。
fn is_jpeg_magic(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// GIF 魔数（`GIF8`）检测：GIF 走全帧动图解码，其余格式单帧静态。以字节为准，
/// 伪装成其它扩展名的 GIF 也会被识别。
fn is_gif_magic(b: &[u8]) -> bool {
    b.starts_with(b"GIF")
}

/// ICO 魔数（ICONDIR：reserved(2)=0 + type(2)=1）检测：ICO 亚 ms 尺寸、降档无关紧要，
/// 保持全尺寸 den=1（避免对多帧/带内嵌 PNG 的 ICO 在解码层意外降档）。
fn is_ico_magic(b: &[u8]) -> bool {
    b.len() >= 4 && b[0] == 0 && b[1] == 0 && b[2] == 1 && b[3] == 0
}

/// PNG 魔数（8 字节签名 `\x89PNG\r\n\x1a\n`）检测：区分「PNG 走展示档降采样解码」与
/// 「其它格式走 image 全解」。以字节为准，比扩展名可信（伪装成 .png 的其它格式不会
/// 被误当 PNG 硬解）。
fn is_png_magic(b: &[u8]) -> bool {
    b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

/// HEIC/HEIF 魔数（ISOBMFF `ftyp` box 判定）检测：区分「HEIF 走展示档降档解码」与
/// 「其余格式走 image 全解」。委托 kimg 的 `guess_format`（内部 `heif_brands` 校验
/// `ftyp` 所列 brand：heic/heix/heim/heis/hevc/hevx/mif1/msf1 且不含 avif/avis），
/// 勿手写易错 brand 解析。以字节为准，比扩展名可信（伪装成其它扩展名的 HEIF 也会
/// 被识别；伪装 .heic 的 AVIF 不会误判成 HEIF）。
fn is_heic_magic(b: &[u8]) -> bool {
    matches!(kimg::guess_format(b), Ok(kimg::ImageFormat::Heic))
}

/// **重型格式**判定（并发上限用）：HEIF/HEIC 即重型——单张 debug 全解秒级、全解 RGBA
/// 数十 MB，并发必须受 [`config::image::HEAVY_DECODE_MAX`] 单独封顶以免长期满核。
fn is_heavy_format(bytes: &[u8]) -> bool {
    is_heic_magic(bytes)
}

/// 路径扩展名是否 jpg/jpeg（升档路由的廉价判据；最终仍以魔数 + `probe` 为准）。
fn path_is_jpeg(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

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
/// 先显它（几 ms）再让展示档真图替换，消除「首帧空等熵解码」。既有库的
/// `extract_embedded_jpeg` 只解 **TIFF-RAW** 容器、`extract_jfif_thumbnail` 只解 **APP0-JFIF**，
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

/// 通用「按展示像素需要选降档分母 {1,2,4,8}」：取**最大**的、让解码尺寸 ≥ 需要尺寸的
/// 分母（缺省 1 = 全解）。JPEG/PNG/HEIF/通用静态（BMP/TIFF/WEBP…）共用一处判档（R5A
/// 收口）。保证一张图只在「解码 ≥ 显示」的前提下尽量省内存——GPU 只会把图缩小、绝不
/// 放大 → 显示**不糊**（行业「decode to display size」地基：放大时需要涨、分母自然
/// 下降，需要 > 原图时才落到全解）。
///
/// 判档前先把「视口需要」**按图像纵横比折算成 contain 实际显示尺寸**：contain 只贴一边，
/// 竖图显示宽度永远到不了视口宽（高度先贴边）、横图/全景图高度到不了视口高。直接拿
/// 「宽高双 ≥ 视口 need」卡档会让竖图被短轴卡死 den=1——全尺寸缓存 + 34MB 纹理白占。
/// 折算后（`eff_w = min(nw, nh·iw/ih)`，`eff_h = min(nh, nw·ih/iw)`）短/长轴都投影到
/// 以图比例对应的目标值，降档后仍 ≥ 实际显示尺寸（不放大、不糊），且当 need 比例已与
/// 图像一致时折算为恒等（既有横图回归不变）。
fn pick_den(iw: u32, ih: u32, need_w: f32, need_h: f32) -> u8 {
    let (nw, nh) = (need_w.max(1.0), need_h.max(1.0));
    let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
    let eff_w = nw.min(nh * iw / ih);
    let eff_h = nh.min(nw * ih / iw);
    for den in [8u8, 4, 2, 1] {
        let w = (iw / den as f32).ceil();
        let h = (ih / den as f32).ceil();
        if w >= eff_w && h >= eff_h {
            return den;
        }
    }
    1
}

/// JPEG 按展示需要**降档解码**：den∈{2,4,8} 优先走自研 `kimg` 的 **DCT 缩放**新路径
/// （[`try_decode_jpeg_scaled`]：熵→half-band IDCT 直出 `ceil/den` 尺寸，消掉「全解
/// 大图再盒式降采样」的 48MB~100MB+ 中间态）；任何失败/不支持 → 无缝回退「全解 RGBA8 +
/// 盒式降采样」旧路径（与改动前逐像素一致，零状态泄漏）。返回 BGRA 字节 + 输出尺寸 +
/// 实际降档分母。失败返回 `Err`（由上层回退 `image` 全解），绝不 panic/硬失败。
fn decode_jpeg_display(
    bytes: &[u8],
    need: (f32, f32),
) -> Result<(Vec<u8>, u32, u32, u8), String> {
    if let Some(out) = try_decode_jpeg_scaled(bytes, need) {
        return Ok(out);
    }
    let img = kimg::load_from_memory(bytes).map_err(|e| format!("JPEG 解码失败：{e}"))?;
    let (iw, ih) = img.dimensions();
    let den = pick_den(iw, ih, need.0, need.1);
    let mut rgba = img.to_rgba8();
    if den == 1 {
        // den=1：全解直出（RGBA→BGRA 交换，尺寸=原始）
        swap_rb_in_place(rgba.as_mut());
        return Ok((rgba.into_raw(), iw, ih, 1));
    }
    let (mut bgra, w, h) = box_downsample_bgra_or_rgba_rayon(rgba.as_raw(), iw, ih, den);
    swap_rb_in_place(bgra.as_mut());
    Ok((bgra, w, h, den))
}

/// 解码展示分配硬上界（≤ `kimg::Limits::default()` 的 `max_alloc` = 512MiB）——不可信头
/// 尺寸（JPEG SOF / PNG IHDR）不得触发超出预算的单次分配（K124）。JPEG scaled 输出与
/// PNG display 缓冲共用同一预算。
const DECODE_DISPLAY_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// JPEG **DCT 缩放**直解尝试：成功返回 `(BGRA, w, h, den)`（与 box 网格同构
/// `ceil(iw/den)×ceil(ih/den)`，顶左对齐，den∈{2,4,8}）；任何失败/不支持/非
/// {2,4,8}/超分配预算 → `None`，由 [`decode_jpeg_display`] 无缝回退「全解+盒式」。
/// 门控（`KIMG_JPEG_SCALED`）整收在 `kimg_jpeg::JpegDecoder::read_image_scaled` 内
/// （不支持 → Unsupported），本入口零耦合：Err → None → 旧路径。
fn try_decode_jpeg_scaled(bytes: &[u8], need: (f32, f32)) -> Option<(Vec<u8>, u32, u32, u8)> {
    // K124：先应用默认 Limits（max_alloc=512MiB，宽高无默认上限）再取尺寸——对齐 PNG
    // display 路径惯例（viewer.rs 内 decode_png_display 同款 `set_limits(Limits::default())`）。
    // `JpegDecoder::from_bytes` 默认 `no_limits()`，若不先套 limits，不可信 SOF 尺寸可驱动
    // 下方 `vec![0; n]` 做出 GB 级分配。
    let mut dec = kimg::codecs::jpeg::JpegDecoder::from_bytes(bytes).ok()?;
    dec.set_limits(kimg::Limits::default()).ok()?;
    let (iw, ih) = dec.dimensions();
    // 只放行 {2,4,8}：den=1 → 走旧路径（全解；不降级）。选档与 [`pick_den`] 同口径。
    let den = pick_den(iw, ih, need.0, need.1);
    if !matches!(den, 2 | 4 | 8) {
        return None;
    }
    let color = dec.color_type();
    let (sw, sh) = dec.scaled_dimensions(den)?;
    let ch = u64::from(color.bytes_per_pixel());
    // 分配前硬上界（K124）：`ceil(iw/den)×ceil(ih/den)×bpp` 须 ≤ 512MiB；超限不分配、回退
    // 全解兜底（全解路径在 limits 内正确拒绝炸弹头）。`checked` 乘法防中间溢出（16-bit
    // SOF 极端 65535×65535 亦被本界拦下：scaled 32768×32768×3 ≈ 3.0GB > 512MiB）。
    let n = u64::from(sw).checked_mul(u64::from(sh))?.checked_mul(ch)?;
    if n > DECODE_DISPLAY_MAX_BYTES {
        return None;
    }
    let mut buf = vec![0u8; n as usize];
    dec.read_image_scaled(den, &mut buf).ok()?;
    // 展开到 RGBA 4 通道（与 box 旧路径 `to_rgba8` 同形），再 swap → BGRA。
    let mut rgba = if color == kimg::ColorType::Rgba8 {
        buf
    } else {
        expand_png_to_rgba8(&buf, sw, sh, color)?
    };
    swap_rb_in_place(rgba.as_mut());
    Some((rgba, sw, sh, den))
}

/// 盒式降采样并行阈值（输入像素数）：低于则用单线程，避免 rayon 线程池分摊/派发开销
/// 反噬小图（小图后处理占比已可忽略）。
const PARALLEL_DOWNSAMPLE_MIN_PX: u64 = 1_000_000;

/// 4 通道盒式降采样（**单线程参考实现**）：`den×den` 源像素块 → 1 输出像素，每通道取均值
/// （**含 alpha**；右/下边缘不满块按实际像素数归一）。输出尺寸 ceil(iw/den)×ceil(ih/den)。
/// 纯后处理、除输出缓冲区外零分配、不 panic（输入尺寸一致由调用方保证）。用在 PNG 展示
/// 档：全量解出 RGBA8 后缩到展示档，只对**缩小后**的缓冲区 swap 红蓝。像素测试以此实现
/// 为基准；生产大图走同语义的 [`box_downsample_bgra_or_rgba_rayon`]（并行与串行逐字节
/// 一致，有回归断言兜底）。
fn box_downsample_bgra_or_rgba(buf: &[u8], iw: u32, ih: u32, den: u8) -> (Vec<u8>, u32, u32) {
    let den = u32::from(den).max(1);
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let mut out = vec![0u8; (ow as usize) * (oh as usize) * 4];
    for oy in 0..oh {
        downsample_row_4ch(buf, iw, ih, den, ow, oy, &mut out[(oy as usize * ow as usize) * 4..]);
    }
    (out, ow, oh)
}

/// 单条输出行：对其 `den×den` 源块的 `ow` 个像素块各自求 4 通道平均（含 alpha，边缘不满
/// 块按实际像素数归一）。行与行互不依赖 → 供 rayon 按行并行复用。
#[inline]
fn downsample_row_4ch(buf: &[u8], iw: u32, ih: u32, den: u32, ow: u32, oy: u32, dst: &mut [u8]) {
    let y0 = oy * den;
    let y1 = (y0 + den).min(ih);
    for ox in 0..ow {
        let x0 = ox * den;
        let x1 = (x0 + den).min(iw);
        let col_bytes = (x1 - x0) as usize * 4;
        let mut acc = [0u32; 4];
        let mut n = 0u32;
        for y in y0..y1 {
            let row = &buf[((y * iw + x0) as usize) * 4..][..col_bytes];
            let mut off = 0usize;
            while off < col_bytes {
                acc[0] += row[off] as u32;
                acc[1] += row[off + 1] as u32;
                acc[2] += row[off + 2] as u32;
                acc[3] += row[off + 3] as u32;
                n += 1;
                off += 4;
            }
        }
        let di = ox as usize * 4;
        if n > 0 {
            dst[di] = (acc[0] / n) as u8;
            dst[di + 1] = (acc[1] / n) as u8;
            dst[di + 2] = (acc[2] / n) as u8;
            dst[di + 3] = (acc[3] / n) as u8;
        }
    }
}

/// 同 [`box_downsample_bgra_or_rgba`] 语义的 **rayon 行组并行** 版：输出行互不依赖，按行
/// 派给并行迭代器；输入像素 < [`PARALLEL_DOWNSAMPLE_MIN_PX`] 时退回单线程（避免线程池
/// 开销反噬小图）。产出与单线程参考实现逐字节一致（整数平均确定性），有回归断言兜底。
fn box_downsample_bgra_or_rgba_rayon(buf: &[u8], iw: u32, ih: u32, den: u8) -> (Vec<u8>, u32, u32) {
    if (iw as u64) * (ih as u64) < PARALLEL_DOWNSAMPLE_MIN_PX {
        return box_downsample_bgra_or_rgba(buf, iw, ih, den);
    }
    use rayon::prelude::*;
    let den = u32::from(den).max(1);
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let row_bytes = (ow as usize) * 4;
    let mut out = vec![0u8; row_bytes * (oh as usize)];
    out.par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(oy, dst)| downsample_row_4ch(buf, iw, ih, den, ow, oy as u32, dst));
    (out, ow, oh)
}

/// 3 通道（RGB8）盒式降采样 → 直接产出 **BGRA**（RGBA 展开 + R/B 交换一步完成，无中间
/// 4 通道缓冲）：`den×den` 源像素块 → 1 输出像素，每通道取均值，输出像素 [B,G,R,255]；
/// 右/下边缘不满块按实际像素数归一。与 [`box_downsample_bgra_or_rgba`] 语义逐字节一致
/// （每通道整数平均 + 通道序置换可交换），仅输入为 3 通道——供 BMP/TIFF/WEBP 等静态
/// 「3 通道原始缓冲先降采样再上屏」省掉全尺寸 RGBA 展开（8.9MP BMP 免 35MB 中间态）。
fn box_downsample_rgb8_to_bgra(buf: &[u8], iw: u32, ih: u32, den: u8) -> (Vec<u8>, u32, u32) {
    let den = u32::from(den).max(1);
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let mut out = vec![0u8; (ow as usize) * (oh as usize) * 4];
    for oy in 0..oh {
        downsample_row_3ch(
            buf, iw, ih, den, ow, oy,
            &mut out[(oy as usize * ow as usize) * 4..],
        );
    }
    (out, ow, oh)
}

/// 单条输出行：对其 `den×den` 源块的 `ow` 个像素块各自求 3 通道平均，直接写 BGRA
/// （B←R / G←G / R←B / A=255）。行与行互不依赖 → 供 rayon 按行并行复用。
#[inline]
fn downsample_row_3ch(buf: &[u8], iw: u32, ih: u32, den: u32, ow: u32, oy: u32, dst: &mut [u8]) {
    let y0 = oy * den;
    let y1 = (y0 + den).min(ih);
    for ox in 0..ow {
        let x0 = ox * den;
        let x1 = (x0 + den).min(iw);
        let col_bytes = (x1 - x0) as usize * 3;
        let mut acc = [0u32; 3];
        let mut n = 0u32;
        for y in y0..y1 {
            let row = &buf[((y * iw + x0) as usize) * 3..][..col_bytes];
            let mut off = 0usize;
            while off < col_bytes {
                acc[0] += row[off] as u32;
                acc[1] += row[off + 1] as u32;
                acc[2] += row[off + 2] as u32;
                n += 1;
                off += 3;
            }
        }
        let di = ox as usize * 4;
        if n > 0 {
            dst[di] = (acc[2] / n) as u8; // B ← 源通道 2（蓝；RGB→BGRA 重排 R/B）
            dst[di + 1] = (acc[1] / n) as u8; // G
            dst[di + 2] = (acc[0] / n) as u8; // R ← 源通道 0（红）
            dst[di + 3] = 255;
        }
    }
}

/// 同 [`box_downsample_rgb8_to_bgra`] 语义的 **rayon 行组并行** 版：输出行互不依赖、按行
/// 派给并行迭代器；输入像素 < [`PARALLEL_DOWNSAMPLE_MIN_PX`] 时退回单线程（与 4 通道
/// 版同为「先判小图单线程」的热路径惯例）。产出与单线程参考实现逐字节一致。
fn box_downsample_rgb8_to_bgra_rayon(buf: &[u8], iw: u32, ih: u32, den: u8) -> (Vec<u8>, u32, u32) {
    if (iw as u64) * (ih as u64) < PARALLEL_DOWNSAMPLE_MIN_PX {
        return box_downsample_rgb8_to_bgra(buf, iw, ih, den);
    }
    use rayon::prelude::*;
    let den = u32::from(den).max(1);
    let ow = iw.div_ceil(den);
    let oh = ih.div_ceil(den);
    let row_bytes = (ow as usize) * 4;
    let mut out = vec![0u8; row_bytes * (oh as usize)];
    out.par_chunks_mut(row_bytes)
        .enumerate()
        .for_each(|(oy, dst)| downsample_row_3ch(buf, iw, ih, den, ow, oy as u32, dst));
    (out, ow, oh)
}

/// PngDecoder 非 RGBA8 输出（灰度 / 灰度+alpha / 16bit）→ RGBA8 展开（每像素 4 字节 RGBA）。
/// PngDecoder 固定 EXPAND 变换：palette 已展开为 Rgb/Rgba、<8bit 已升到 8bit，但 16bit
/// 通道仍为 L16/La16/Rgb16/Rgba16（`read_image` 已按契约转本机字节序）——这里统一按
/// 高字节右移 8 归一为 u8（与 image 16→8 缩放惯例一致）。
fn expand_png_to_rgba8(buf: &[u8], w: u32, h: u32, color: kimg::ColorType) -> Option<Vec<u8>> {
    let n = (w as usize).checked_mul(h as usize)?;
    let mut out = vec![0u8; n * 4];
    match color {
        kimg::ColorType::Rgba8 => {
            if buf.len() != n * 4 {
                return None;
            }
            out.copy_from_slice(buf);
        }
        kimg::ColorType::Rgb8 => {
            if buf.len() != n * 3 {
                return None;
            }
            for (src, dst) in buf.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
                dst[..3].copy_from_slice(src);
                dst[3] = 255;
            }
        }
        kimg::ColorType::L8 => {
            if buf.len() != n {
                return None;
            }
            for (g, dst) in buf.iter().zip(out.chunks_exact_mut(4)) {
                dst[0] = *g;
                dst[1] = *g;
                dst[2] = *g;
                dst[3] = 255;
            }
        }
        kimg::ColorType::La8 => {
            if buf.len() != n * 2 {
                return None;
            }
            for (p, dst) in buf.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
                dst[0] = p[0];
                dst[1] = p[0];
                dst[2] = p[0];
                dst[3] = p[1];
            }
        }
        kimg::ColorType::Rgba16 => {
            if buf.len() != n * 8 {
                return None;
            }
            for (p, dst) in buf.chunks_exact(8).zip(out.chunks_exact_mut(4)) {
                dst[0] = (u16::from_ne_bytes([p[0], p[1]]) >> 8) as u8;
                dst[1] = (u16::from_ne_bytes([p[2], p[3]]) >> 8) as u8;
                dst[2] = (u16::from_ne_bytes([p[4], p[5]]) >> 8) as u8;
                dst[3] = (u16::from_ne_bytes([p[6], p[7]]) >> 8) as u8;
            }
        }
        kimg::ColorType::Rgb16 => {
            if buf.len() != n * 6 {
                return None;
            }
            for (p, dst) in buf.chunks_exact(6).zip(out.chunks_exact_mut(4)) {
                dst[0] = (u16::from_ne_bytes([p[0], p[1]]) >> 8) as u8;
                dst[1] = (u16::from_ne_bytes([p[2], p[3]]) >> 8) as u8;
                dst[2] = (u16::from_ne_bytes([p[4], p[5]]) >> 8) as u8;
                dst[3] = 255;
            }
        }
        kimg::ColorType::L16 => {
            if buf.len() != n * 2 {
                return None;
            }
            for (p, dst) in buf.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
                let g = (u16::from_ne_bytes([p[0], p[1]]) >> 8) as u8;
                dst[0] = g;
                dst[1] = g;
                dst[2] = g;
                dst[3] = 255;
            }
        }
        kimg::ColorType::La16 => {
            if buf.len() != n * 4 {
                return None;
            }
            for (p, dst) in buf.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                let g = (u16::from_ne_bytes([p[0], p[1]]) >> 8) as u8;
                let a = (u16::from_ne_bytes([p[2], p[3]]) >> 8) as u8;
                dst[0] = g;
                dst[1] = g;
                dst[2] = g;
                dst[3] = a;
            }
        }
        _ => return None, // PngDecoder 不会产出 float32 等类型（防御）
    }
    Some(out)
}

/// PNG 展示解码**分配预算判定**（K124）：IHDR 声明尺寸驱动的像素字节数（`total_bytes`）
/// 超过 [`DECODE_DISPLAY_MAX_BYTES`] → 分配前拒绝（对齐 JPEG scaled 预检姿势）。纯函数，
/// 便于单测「预检触发」。
fn png_decode_within_budget(nbytes: u64) -> bool {
    nbytes <= DECODE_DISPLAY_MAX_BYTES
}

/// PNG 按展示需要**降档解码**（`kimg::codecs::png::PngDecoder` 解全量位图，非 RGBA8 先
/// 展开，再对全量做盒式降采样到展示档）：返回 BGRA 字节 + 输出尺寸 + 实际降档分母。
/// 选档复用 [`pick_den`]（与格式无关的「解码 ≥ 显示」语义）。
/// - den=1 → 与改动前 `image` 全解**逐像素一致**（RGBA swap 成 BGRA，尺寸=原始）；
/// - den>1 → 只对**缩小后**缓冲区 swap 红蓝（不在 9M 全量上做交换）。
/// 任何失败均返回 `Err`（由上层回退 `image` 全解），绝不 panic/硬失败。
fn decode_png_display(
    bytes: &[u8],
    need: (f32, f32),
) -> Result<(Vec<u8>, u32, u32, u8), String> {
    let mut dec = kimg::codecs::png::PngDecoder::new(Cursor::new(bytes))
        .map_err(|e| format!("PNG 解码器初始化失败：{e}"))?;
    // 与 `image` 全解一致的上限护栏（Limits::default()：max_alloc=512MB / 最大宽高 65535）
    dec.set_limits(kimg::Limits::default())
        .map_err(|e| format!("PNG 尺寸超限：{e}"))?;
    let (iw, ih) = dec.dimensions();
    let den = pick_den(iw, ih, need.0, need.1);
    let color = dec.color_type();
    let nbytes = usize::try_from(dec.total_bytes()).map_err(|_| "PNG 解码尺寸过大".to_string())?;
    // K124 分配前硬上界（对齐 JPEG scaled 预检姿势）：IHDR 声明尺寸驱动的 `total_bytes`
    // 超预算 → 不分配、返回 `Err`（解码池回退 `image` 全解，其内部同样套默认 `Limits`、
    // 对超大声明干净拒绝，绝不 OOM/GB 级分配）。
    if !png_decode_within_budget(nbytes as u64) {
        return Err("PNG 解码尺寸超过预算".to_string());
    }
    let mut raw = vec![0u8; nbytes];
    dec.read_image(&mut raw)
        .map_err(|e| format!("PNG 解码失败：{e}"))?;
    // 8bit RGBA 直接复用读出缓冲（零拷贝）；其余色彩逐一展开到 RGBA8
    let mut rgba = if color == kimg::ColorType::Rgba8 {
        raw
    } else {
        expand_png_to_rgba8(&raw, iw, ih, color).ok_or("PNG 色彩转换失败")?
    };
    if den == 1 {
        // den=1：与改动前 `image` 全解逐像素一致（RGBA→BGRA 交换、尺寸=原始）
        swap_rb_in_place(rgba.as_mut());
        return Ok((rgba, iw, ih, 1));
    }
    let (mut bgra, w, h) = box_downsample_bgra_or_rgba_rayon(&rgba, iw, ih, den);
    swap_rb_in_place(bgra.as_mut());
    Ok((bgra, w, h, den))
}

/// HEIF/HEIC 按展示需要**降档解码**（kimg_heif 全解出 RGBA 帧 → 与 PNG/JPEG 同款盒式
/// 降采样到展示档）：返回 BGRA 字节 + 输出尺寸 + 实际降档分母。选档复用
/// [`pick_den`]（与格式无关的「解码 ≥ 显示」语义）。全解路径沿用
/// [`decode_static_first_frame`] 同源（`kimg::load_from_memory`），输出**已含显示朝向
/// 变换**。
/// - den=1 → 与改动前 `decode_generic` 全解**逐像素一致**（RGBA swap 成 BGRA，尺寸=原始）；
/// - den>1 → 只对**缩小后**缓冲区 swap 红蓝（不在大图上做交换）。
/// 任何失败均返回 `Err`（由上层回退 `decode_generic` 全解 den=1，绝不 panic/硬失败）。
fn decode_heif_display(
    bytes: &[u8],
    need: (f32, f32),
) -> Result<(Vec<u8>, u32, u32, u8), String> {
    let img = kimg::load_from_memory(bytes).map_err(|e| format!("HEIF 解码失败：{e}"))?;
    let (iw, ih) = img.dimensions();
    let den = pick_den(iw, ih, need.0, need.1);
    let mut rgba = img.to_rgba8();
    if den == 1 {
        // den=1：全解直出（RGBA→BGRA 交换，尺寸=原始；与 decode_generic 逐像素一致）
        swap_rb_in_place(rgba.as_mut());
        return Ok((rgba.into_raw(), iw, ih, 1));
    }
    let (mut bgra, w, h) = box_downsample_bgra_or_rgba_rayon(rgba.as_raw(), iw, ih, den);
    swap_rb_in_place(bgra.as_mut());
    Ok((bgra, w, h, den))
}

/// 其余静态格式（BMP/TIFF/WEBP/PNM…）按展示需要**降档解码**（R5）：kimg 全解出**原始
/// 缓冲**（按其通道：BMP 3ch RGB8 / TIFF / WEBP 等）→ [`pick_den`] 通用判档 → **先对
/// 原始缓冲盒式降采样**到展示档再上屏。返回 (BGRA, 输出宽, 输出高, 降档分母)。
/// - **GIF 动画保持 den=1**（动画帧必须全量驻留才能播；R5 范围墙：动画帧降宽容错面大，
///   本任务不做）；**ICO 亚 ms 尺寸保持 den=1**（尺寸小无关紧要）；
/// - 通道分支：3ch RGB8 → [`box_downsample_rgb8_to_bgra_rayon`] 直出 BGRA（免全尺寸
///   RGBA 展开）；4ch RGBA8 → 复用 4 通道盒式后 swap；其余（L8/La8/16bit 灰度 TIFF…）
///   先 [`expand_png_to_rgba8`] 展开再走 4 通道盒式；
/// - den=1 → 与改动前 [`decode_static_first_frame`] 全解**逐像素一致**（RGBA→BGRA swap，
///   尺寸=原始）；
/// - den>1 → 只在**降档后**缓冲上做通道重排（不在大图上做交换）。
/// 任何失败均返回 `Err`（由上层回退 [`decode_generic`] 全解 den=1，绝不 panic/硬失败）。
fn decode_generic_display(
    bytes: &[u8],
    need: (f32, f32),
) -> Result<(Vec<u8>, u32, u32, u8), String> {
    let img = kimg::load_from_memory(bytes).map_err(|e| format!("无法解码图片：{e}"))?;
    let (iw, ih) = img.dimensions();
    if is_ico_magic(bytes) {
        // ICO 亚 ms 尺寸无关紧要 → 全尺寸 den=1（与 [decode_static_first_frame] 逐像素一致）
        let mut rgba = img.to_rgba8();
        swap_rb_in_place(rgba.as_mut());
        return Ok((rgba.into_raw(), iw, ih, 1));
    }
    let den = pick_den(iw, ih, need.0, need.1);
    if den == 1 {
        // den=1：全解直出（RGBA→BGRA 交换，尺寸=原始；与 decode_static_first_frame 逐像素一致）
        let mut rgba = img.to_rgba8();
        swap_rb_in_place(rgba.as_mut());
        return Ok((rgba.into_raw(), iw, ih, 1));
    }
    // den>1：按解码原通道「先降采样再上屏」
    match img.color() {
        kimg::ColorType::Rgb8 => {
            let (bgra, w, h) = box_downsample_rgb8_to_bgra_rayon(img.as_bytes(), iw, ih, den);
            Ok((bgra, w, h, den))
        }
        kimg::ColorType::Rgba8 => {
            let (mut bgra, w, h) = box_downsample_bgra_or_rgba_rayon(img.as_bytes(), iw, ih, den);
            swap_rb_in_place(bgra.as_mut());
            Ok((bgra, w, h, den))
        }
        // 其它通道（L8/La8/16bit 等，如灰度 TIFF）：先展开 RGBA8 再走 4 通道盒式
        _ => {
            let rgba = expand_png_to_rgba8(img.as_bytes(), iw, ih, img.color())
                .ok_or("非 8bit 色彩展开失败")?;
            let (mut bgra, w, h) = box_downsample_bgra_or_rgba_rayon(&rgba, iw, ih, den);
            swap_rb_in_place(bgra.as_mut());
            Ok((bgra, w, h, den))
        }
    }
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
    let img = kimg::load_from_memory(bytes).map_err(|e| format!("无法解码图片：{e}"))?;
    let mut rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    swap_rb_in_place(rgba.as_mut());
    Ok((vec![Frame::new(rgba)], w, h))
}

/// GIF 全帧解码失败 → 回退「首帧静态」（与改前 behaviour 一致，绝不硬失败）；静态
/// 也解不动时返回原始 GIF 错误（顶层仍以加载错误呈现，不 panic）。
fn decode_static_first_frame_or(
    bytes: &[u8],
    gif_err: kimg::ImageError,
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
    let mut decoder = kimg::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .map_err(|e| format!("无法解码图片：{e}"))?;
    // 资源护栏①：套上 image 默认 Limits（max_alloc=512MB）——`GifDecoder::new` 内部是
    // `Limits::no_limits()`，直接 `into_frames` 会在首个帧前就按逻辑屏画布预分配无上限
    // 的 RGBA 画布（荒谬超大逻辑屏描述符可瞬间吃爆内存）；默认 limits 在 `reserve` 阶段
    // 用 512MB 单分配预算拦下，超额以 `ImageResult` 报错，不 panic。
    decoder
        .set_limits(kimg::Limits::default())
        .map_err(|e| format!("无法解码图片：{e}"))?;
    let (w, h) = decoder.dimensions();
    let frames = match decoder
        .into_frames()
        .take(MAX_GIF_FRAMES)
        .collect::<kimg::ImageResult<Vec<_>>>()
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

/// 后台解码总入口：JPEG → 自研 `kimg` 全解后按展示需要盒式降档出 BGRA（特殊 JPEG 如
/// CMYK / 12bit / lossless 解不动时回退 `image` 全解，绝不硬失败）包成单帧；PNG → 展示档
/// 降采样解码（[`decode_png_display`]，失败回退 `image` 全解）；HEIF/HEIC → 展示档降档
/// 解码（[`decode_heif_display`]：kimg_heif 全解出 RGBA → 盒式降采样，失败回退全解）；
/// 其余静态格式（BMP/TIFF/WEBP/ICO…）→ 展示档降档（[`decode_generic_display`]，先盒式
/// 降采样原始缓冲再上屏，失败回退全解）；GIF → 全帧动图（den=1）。返回
/// (BGRA 帧序列, 宽, 高, 降档分母)。den=1 时与改动前逐像素一致。
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
    } else if is_png_magic(bytes) {
        match decode_png_display(bytes, need) {
            Ok((bgra, w, h, den)) => {
                let frame = bgra_single_frame(bgra, w, h).ok_or("解码结果尺寸异常")?;
                Ok((vec![frame], w, h, den))
            }
            Err(_) => {
                // 降采样链路任何失败 → 回退既有 `image` 全解（den=1），绝不硬失败/panic
                let (frames, w, h) = decode_generic(bytes)?;
                Ok((frames, w, h, 1))
            }
        }
    } else if is_heic_magic(bytes) {
        match decode_heif_display(bytes, need) {
            Ok((bgra, w, h, den)) => {
                let frame = bgra_single_frame(bgra, w, h).ok_or("解码结果尺寸异常")?;
                Ok((vec![frame], w, h, den))
            }
            Err(_) => {
                // 降档链路任何失败 → 回退既有 `image` 全解（den=1），绝不硬失败/panic
                let (frames, w, h) = decode_generic(bytes)?;
                Ok((frames, w, h, 1))
            }
        }
    } else if is_gif_magic(bytes) {
        // GIF 动画保持全尺寸 den=1（动画帧必须全量驻留才能播；R5 范围墙：动画帧降宽
        // 容错面大、本任务不做）
        let (frames, w, h) = decode_generic(bytes)?;
        Ok((frames, w, h, 1))
    } else {
        // 其余静态格式（BMP/TIFF/WEBP/ICO/PNM…）：展示档降档解码（R5）。降档链路任何
        // 失败 → 回退既有 `decode_generic` 全解（den=1），绝不硬失败/panic。
        match decode_generic_display(bytes, need) {
            Ok((bgra, w, h, den)) => {
                let frame = bgra_single_frame(bgra, w, h).ok_or("解码结果尺寸异常")?;
                Ok((vec![frame], w, h, den))
            }
            Err(_) => {
                let (frames, w, h) = decode_generic(bytes)?;
                Ok((frames, w, h, 1))
            }
        }
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
    use kimg::ImageBufferExt;

    /// 每测试独立的临时目录（按进程 id + 标签隔离，避免并行测试互相踩）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vuick-img-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一张真实 PNG（`image` 默认特性可编码；JPEG 不支持 RGBA 编码，故其它
    /// 扩展名用例直接用占位字节——扫描只看扩展名，不看内容）。
    fn write_png(path: &Path, w: u32, h: u32) {
        let img = kimg::RgbaImage::from_pixel(w, h, kimg::Rgba([200_u8, 30, 10, 255]));
        img.save(path).expect("本测试自产 PNG 应能写出");
    }

    /// 手写 **24bit 无压缩 BMP**（BI_RGB，自底向上）+ 4 字节行填充：kimg 仅 PNG/GIF
    /// 可编码，BMP/TIFF/WEBP 夹具须手写。kimg_bmp 解码 BMP 为 3ch RGB8 —— R5 通用静态
    /// display 降档（3 通道先降采样再上屏）的确定性夹具。`color(x,y)` 供渐变/角点断言。
    fn write_bmp(path: &Path, w: u32, h: u32, color: impl Fn(u32, u32) -> [u8; 3]) {
        let stride = ((w * 3 + 3) / 4) * 4;
        let px_off = 54u32;
        let file_size = px_off + stride * h;
        let mut out = Vec::with_capacity(file_size as usize);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&file_size.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // 保留
        out.extend_from_slice(&px_off.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER 尺寸
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes()); // 正 = 自底向上
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&24u16.to_le_bytes()); // bpp
        out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        out.extend_from_slice(&[0u8; 20]); // 图像大小(4)+Xppm(4)+Yppm(4)+颜色数(4)+重要颜色(4)
        for y in (0..h).rev() {
            let mut row = Vec::with_capacity(stride as usize);
            for x in 0..w {
                let [r, g, b] = color(x, y);
                row.extend_from_slice(&[b, g, r]); // BMP 像素序 BGR
            }
            row.resize(stride as usize, 0);
            out.extend_from_slice(&row);
        }
        std::fs::write(path, out).expect("写出 BMP 应成功");
    }

    /// 手写**最小无压缩 RGB8 TIFF**（单 strip、top-down、Photometric=2=RGB）：IFD 9 项 +
    /// BitsPerSample 数组 + 原始 RGB8 像素。kimg_tiff 解码为 3ch RGB8 —— 与 BMP 同走
    /// R5 通用静态降档路径。`color(x,y)` 供渐变断言。
    fn write_rgb8_tiff(path: &Path, w: u32, h: u32, color: impl Fn(u32, u32) -> [u8; 3]) {
        let entries: u16 = 9;
        let bits_off: u32 = 8 + 2 + (entries as u32) * 12 + 4;
        let data_off: u32 = (bits_off + 6).div_ceil(4) * 4;
        let n = w as usize * h as usize * 3;
        let mut out = Vec::with_capacity(data_off as usize + n);
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes()); // IFD 偏移
        out.extend_from_slice(&entries.to_le_bytes());
        let mut push = |tag: u16, ty: u16, count: u32, val: u32| {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        };
        push(256, 4, 1, w); // ImageWidth
        push(257, 4, 1, h); // ImageLength
        push(258, 3, 3, bits_off); // BitsPerSample(8,8,8)
        push(259, 3, 1, 1); // Compression = none
        push(262, 3, 1, 2); // Photometric = RGB
        push(273, 4, 1, data_off); // StripOffsets
        push(277, 3, 1, 3); // SamplesPerPixel
        push(278, 4, 1, h); // RowsPerStrip
        push(279, 4, 1, n as u32); // StripByteCounts
        out.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 无
        for _ in 0..3 {
            out.extend_from_slice(&8u16.to_le_bytes());
        }
        out.resize(data_off as usize, 0);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = color(x, y);
                out.extend_from_slice(&[r, g, b]);
            }
        }
        std::fs::write(path, out).expect("写出 TIFF 应成功");
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

    /// 手写极小 PNG 字节（免依赖）：CRC32 + chunk 拼装 + deflate **stored** 块（无压缩，
    /// 避开对 flate2/zlib 的直接依赖）。
    fn png_adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }

    fn png_chunk_byte(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        fn crc32(d: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in d {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
                }
            }
            !c
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32(&out[4..]).to_be_bytes());
        out
    }

    /// 手写 1×1 RGBA8 PNG；`interlace`=1 走 Adam7（1×1 仅 pass0 一个扫描行，IDAT 载荷与
    /// 非交错相同）。覆盖「交错 PNG PngDecoder 自动重组、至少不 panic」。
    fn minimal_1x1_rgba_png(interlace: u8, pixel: [u8; 4]) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, interlace]); // 8bit RGBA, interlace 标志
        out.extend_from_slice(&png_chunk_byte(b"IHDR", &ihdr));
        let scan: Vec<u8> = std::iter::once(0).chain(pixel).collect(); // filter0 + RGBA
        let mut idat = Vec::new();
        idat.extend_from_slice(&[0x78, 0x01]); // zlib 头（CMF/FLG，FCHECK 整除 31）
        idat.push(0x01); // deflate stored 块：BFINAL=1, BTYPE=00
        idat.extend_from_slice(&(scan.len() as u16).to_le_bytes());
        idat.extend_from_slice(&(!(scan.len() as u16)).to_le_bytes());
        idat.extend_from_slice(&scan);
        idat.extend_from_slice(&png_adler32(&scan).to_be_bytes()); // zlib adler32 尾部
        out.extend_from_slice(&png_chunk_byte(b"IDAT", &idat));
        out.extend_from_slice(&png_chunk_byte(b"IEND", &[]));
        out
    }

    /// 手写 2×1 palette PNG（8bit 索引色）：IHDR colortype=3 + PLTE（两色）+ IDAT 两个索引
    /// 字节（stored deflate，无 tRNS → alpha=255）。验证 palette 被 PngDecoder EXPAND 展开
    /// （image 编码器不产 palette 文件，故手写构造）。
    fn minimal_2x1_palette_png(colors: &[[u8; 3]], indices: &[u8]) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 3, 0, 0, 0]); // 8bit 索引色
        out.extend_from_slice(&png_chunk_byte(b"IHDR", &ihdr));
        let plte: Vec<u8> = colors.iter().flatten().copied().collect();
        out.extend_from_slice(&png_chunk_byte(b"PLTE", &plte));
        let mut scan = vec![0u8]; // filter0
        scan.extend_from_slice(indices);
        let mut idat = Vec::new();
        idat.extend_from_slice(&[0x78, 0x01]); // zlib 头
        idat.push(0x01);
        idat.extend_from_slice(&(scan.len() as u16).to_le_bytes());
        idat.extend_from_slice(&(!(scan.len() as u16)).to_le_bytes());
        idat.extend_from_slice(&scan);
        idat.extend_from_slice(&png_adler32(&scan).to_be_bytes()); // zlib adler32 尾部
        out.extend_from_slice(&png_chunk_byte(b"IDAT", &idat));
        out.extend_from_slice(&png_chunk_byte(b"IEND", &[]));
        out
    }

    /// 手写**多 pass** 交错（Adam7）PNG（实心色，任意尺寸）：按 PNG 规范把各 pass 的滤波
    /// 扫描行按 pass 顺序拼进 IDAT（stored deflate + adler32）。实心色让所有 pass 的像素
    /// 相同、无需逐像素推算各 pass 归属，解码后整幅必须全是该色，从而验证 PngDecoder 的
    /// Adam7 重组正确（不只 1×1 单 pass）。字节已用 python zlib + Pillow 交叉验证。
    fn minimal_interlaced_solid_png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        // Adam7 七 pass 几何：(起始行, 行间距, 起始列, 列间距)
        let passes: [(u32, u32, u32, u32); 7] = [
            (0, 8, 0, 8),
            (0, 8, 4, 8),
            (4, 8, 0, 4),
            (0, 4, 2, 4),
            (2, 4, 0, 2),
            (0, 2, 1, 2),
            (1, 2, 0, 1),
        ];
        let mut raw = Vec::new();
        for (pr, pg, pc, cg) in passes {
            let rows = if h > pr { (h - pr - 1) / pg + 1 } else { 0 };
            let cols = if w > pc { (w - pc - 1) / cg + 1 } else { 0 };
            if rows > 0 && cols > 0 {
                let mut line = vec![0u8]; // filter0
                for _ in 0..cols {
                    line.extend_from_slice(&color);
                }
                for _ in 0..rows {
                    raw.extend_from_slice(&line);
                }
            }
        }
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 1]); // 8bit RGBA, Adam7
        out.extend_from_slice(&png_chunk_byte(b"IHDR", &ihdr));
        let mut idat = Vec::new();
        idat.extend_from_slice(&[0x78, 0x01]);
        idat.push(0x01);
        idat.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        idat.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        idat.extend_from_slice(&raw);
        idat.extend_from_slice(&png_adler32(&raw).to_be_bytes());
        out.extend_from_slice(&png_chunk_byte(b"IDAT", &idat));
        out.extend_from_slice(&png_chunk_byte(b"IEND", &[]));
        out
    }

    /// 极小图（40×20 RGBA8，den=1）：decode_png_display 的 BGRA 输出与改动前 `image` 全解
    /// （decode_static_first_frame）**逐像素一致**；尺寸/字节数不变。
    #[test]
    fn decode_png_display_den1_equals_old_full_decode_pixel_perfect() {
        let dir = tmp_dir("png_den1");
        let p = dir.join("p.png");
        write_png(&p, 40, 20);
        let bytes = std::fs::read(&p).unwrap();
        // need 远超原图 → 恒 den=1（全解档）
        let (bgra, w, h, den) = decode_png_display(&bytes, (1000.0, 1000.0)).unwrap();
        assert_eq!((w, h, den), (40, 20, 1));
        let (frames, w2, h2) = decode_static_first_frame(&bytes).unwrap();
        assert_eq!((w2, h2), (40, 20));
        let old = frames[0].buffer().as_raw();
        assert_eq!(bgra, *old, "den=1 输出必须与改动前全解逐像素一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 尺寸断言：3000×2400 需要 (1032,1032) → den=2，输出 1500×1200 ≥ need（不糊，对齐
    /// pick_den_never_below_display_pixels 的「输出 ≥ 显示需要」同款断言）。
    #[test]
    fn decode_png_display_downscales_and_never_below_display_pixels() {
        let dir = tmp_dir("png_den2");
        let p = dir.join("big.png");
        write_png(&p, 3000, 2400);
        let bytes = std::fs::read(&p).unwrap();
        let (bgra, w, h, den) = decode_png_display(&bytes, (1032.0, 1032.0)).unwrap();
        assert_eq!(den, 2, "3000×2400 对 1032 需要应选 1/2");
        assert_eq!((w, h), (1500, 1200));
        assert!(w >= 1032 && h >= 1032, "解码像素不得低于显示需要");
        assert_eq!(bgra.len(), (w as usize * h as usize * 4) as usize);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 极大边界：den=4 与 den=8 且奇数宽（2999 → 右/下缘不满块，归一仍正确）。
    #[test]
    fn decode_png_display_den4_and_den8_odd_dims() {
        let dir = tmp_dir("png_odd");
        let p = dir.join("a.png");
        write_png(&p, 2999, 2400);
        let bytes = std::fs::read(&p).unwrap();
        // 需要 (400,320)（≈ 图 4:5 纵横比 → R5A 按纵横比折算近恒等）：den8(375×300) < 需要
        // → 落 den4：750×600（ceil(2999/4)=750）
        let (_, w4, h4, den4) = decode_png_display(&bytes, (400.0, 320.0)).unwrap();
        assert_eq!(den4, 4);
        assert_eq!((w4, h4), (750, 600));
        assert!(w4 >= 400 && h4 >= 320);
        let (_, w8, h8, den8) = decode_png_display(&bytes, (100.0, 60.0)).unwrap();
        assert_eq!(den8, 8);
        assert_eq!((w8, h8), (375, 300)); // ceil(2999/8)=375, ceil(2400/8)=300
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 像素断言：盒式降采样对 4 通道各自求平均（2×2 半红半蓝 → 平均 0x7F,0,0x7F；
    /// 含 alpha 平均；单色块输出同色）。
    #[test]
    fn box_downsample_averages_channels_and_alpha() {
        // RGBA 2×2：上排红、下排蓝（红=FF0000FF，蓝=0000FFFF）→ 平均 (127,0,127,255)
        let buf: Vec<u8> = [
            [255, 0, 0, 255], [255, 0, 0, 255],
            [0, 0, 255, 255], [0, 0, 255, 255],
        ]
        .concat();
        let (out, w, h) = box_downsample_bgra_or_rgba(&buf, 2, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[..4], &[127, 0, 127, 255]);
        // 单色块 → 输出同色
        let flat: Vec<u8> = [100u8, 150, 200, 42].repeat(4);
        let (out2, _, _) = box_downsample_bgra_or_rgba(&flat, 2, 2, 2);
        assert_eq!(&out2[..4], &[100, 150, 200, 42]);
        // alpha 平均：2×2 两个 alpha=0 两个 =255 → 平均 127；RGB 全同
        let alpha: Vec<u8> = [
            [10, 20, 30, 0], [10, 20, 30, 255],
            [10, 20, 30, 0], [10, 20, 30, 255],
        ]
        .concat();
        let (out3, _, _) = box_downsample_bgra_or_rgba(&alpha, 2, 2, 2);
        assert_eq!(&out3[..4], &[10, 20, 30, 127]);
    }

    /// 边缘不满块按实际像素数归一：3×2 den=2 → 输出 2×1；右列只 2 像素参与平均（非整除）。
    #[test]
    fn box_downsample_partial_edge_blocks_use_real_pixel_count() {
        // 3×2 布局（RGBA）：a b c / d e f；den=2 → 2×1
        // 块(0)=a,b,d,e 平均；块(1)=c,f 平均（竖排 2 像素，非 4）
        let mk = |r: u8, g: u8, b: u8| [r, g, b, 255];
        let buf: Vec<u8> = [
            mk(0, 0, 0), mk(10, 0, 0), mk(200, 0, 0), // a b c
            mk(0, 20, 0), mk(10, 20, 0), mk(200, 40, 0), // d e f
        ]
        .concat();
        let (out, w, h) = box_downsample_bgra_or_rgba(&buf, 3, 2, 2);
        assert_eq!((w, h), (2, 1));
        // 块(0)：a(0,0,0)+b(10,0,0)+d(0,20,0)+e(10,20,0) → (20,40,0)/4=(5,10,0)
        assert_eq!(&out[0..4], &[5, 10, 0, 255]);
        // 块(1)：c(200,0,0)+f(200,40,0) → (400,40,0)/2=(200,20,0)
        assert_eq!(&out[4..8], &[200, 20, 0, 255]);
    }

    /// rayon 行组并行降采样 == 单线程参考（大缓冲触发并行路径、小缓冲退回串行路径）：
    /// 整数平均确定性 → 应逐字节一致（防并行实现语义漂移，K15 式实证断言）。
    #[test]
    fn parallel_rayon_downsample_matches_serial_reference() {
        // 1.28M 输入像素 > PARALLEL_DOWNSAMPLE_MIN_PX → 并行路径；确定性渐变防「恰好相等」
        let (w, h) = (1600u32, 800u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for (i, px) in buf.chunks_exact_mut(4).enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            px[0] = ((x * 7 + y * 3) % 256) as u8;
            px[1] = ((x * 5 + y * 11) % 256) as u8;
            px[2] = ((x * 13) % 256) as u8;
            px[3] = ((x + y) % 256) as u8;
        }
        let (ser, sw, sh) = box_downsample_bgra_or_rgba(&buf, w, h, 2);
        let (par, pw, ph) = box_downsample_bgra_or_rgba_rayon(&buf, w, h, 2);
        assert_eq!((sw, sh), (pw, ph));
        assert_eq!(ser, par, "并行与串行必须逐字节一致");
        // 小图（低于阈值）走串行回退，也应正常
        let small = vec![9u8; 32 * 32 * 4];
        let (a, aw, ah) = box_downsample_bgra_or_rgba(&small, 32, 32, 2);
        let (b, bw, bh) = box_downsample_bgra_or_rgba_rayon(&small, 32, 32, 2);
        assert_eq!((aw, ah), (bw, bh));
        assert_eq!(a, b);
    }

    /// palette PNG：PngDecoder (EXPAND) 应产出 RGBA8——红蓝两像素、alpha=255。
    #[test]
    fn handcrafted_palette_png_decodes_to_rgba8() {
        let bytes = minimal_2x1_palette_png(&[[255, 0, 0], [0, 0, 255]], &[0, 1]);
        let (bgra, w, h, den) = decode_png_display(&bytes, (1000.0, 1000.0)).unwrap();
        assert_eq!((w, h, den), (2, 1, 1));
        // BGRA 输出：像素0=红 → [B,G,R,A]=[0,0,255,255]；像素1=蓝 → [255,0,0,255]
        assert_eq!(&bgra[0..4], &[0, 0, 255, 255]);
        assert_eq!(&bgra[4..8], &[255, 0, 0, 255]);
    }

    /// 交错（Adam7）与普通 1×1 PNG：PngDecoder 自动重组，不 panic 且像素正确；另加
    /// 8×8 **多 pass** 实心图验证 Adam7 七个 pass 全部正确归位（字节经 python zlib+Pillow 校验）。
    #[test]
    fn handcrafted_interlaced_png_decodes_without_panic() {
        let bytes = minimal_1x1_rgba_png(1, [255, 0, 0, 255]);
        let (bgra, w, h, den) = decode_png_display(&bytes, (1000.0, 1000.0)).unwrap();
        assert_eq!((w, h, den), (1, 1, 1));
        assert_eq!(&bgra[..4], &[0, 0, 255, 255]); // 纯红 → BGRA [B=0,G=0,R=255,A=255]
        // 多 pass Adam7：8×8 实心墨绿，全幅必须都是该色 → 全部 pass 正确重组
        let big = minimal_interlaced_solid_png(8, 8, [0, 128, 0, 255]);
        let (b8, w8, h8, _) = decode_png_display(&big, (1000.0, 1000.0)).unwrap();
        assert_eq!((w8, h8), (8, 8));
        assert!(
            b8.chunks_exact(4).all(|p| p == [0, 128, 0, 255]),
            "Adam7 全 pass 重组后应整幅同色"
        );
        let plain = minimal_1x1_rgba_png(0, [0, 255, 0, 255]);
        let (b2, _, _, _) = decode_png_display(&plain, (1000.0, 1000.0)).unwrap();
        assert_eq!(&b2[..4], &[0, 255, 0, 255]);
    }

    /// 灰度（L8）与 16bit（L16）PNG：PngDecoder 输出非 RGBA8 → 展开为 RGBA8，R=G=B。
    #[test]
    fn gray_and_16bit_png_expand_to_rgba8() {
        let dir = tmp_dir("png_ex");
        let g8 = dir.join("g8.png");
        let img: kimg::ImageBuffer<kimg::Luma<u8>, Vec<u8>> =
            kimg::ImageBuffer::from_pixel(4, 3, kimg::Luma([128]));
        img.save(&g8).expect("写出灰度 PNG 应成功");
        let bytes8 = std::fs::read(&g8).unwrap();
        let (b8, w8, h8, _) = decode_png_display(&bytes8, (1000.0, 1000.0)).unwrap();
        assert_eq!((w8, h8), (4, 3));
        assert_eq!(b8.len(), 4 * 3 * 4);
        assert_eq!([b8[0], b8[1], b8[2], b8[3]], [128, 128, 128, 255], "灰度 → R=G=B");
        let g16 = dir.join("g16.png");
        let img16: kimg::ImageBuffer<kimg::Luma<u16>, Vec<u16>> =
            kimg::ImageBuffer::from_pixel(2, 2, kimg::Luma([0xABCDu16]));
        img16.save(&g16).expect("写出 16bit 灰度 PNG 应成功");
        let bytes16 = std::fs::read(&g16).unwrap();
        let (b16, w16, h16, _) = decode_png_display(&bytes16, (1000.0, 1000.0)).unwrap();
        assert_eq!((w16, h16), (2, 2));
        assert_eq!([b16[0], b16[1], b16[2], b16[3]], [0xAB, 0xAB, 0xAB, 255], "16bit 高字节归一");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非法 PNG 字节：decode_background 不 panic——PNG 魔数命中但无有效 IHDR → 降采样解不了
    /// → 回退全解也失败 → 返回 Err（顶层以加载错误呈现；对齐 decode_missing_file_reports_error
    /// 风格）。完整可解 PNG 不应被误伤。
    #[test]
    fn invalid_png_bytes_fall_back_to_error_not_panic() {
        let bad = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x00];
        assert!(decode_background(&bad, (1000.0, 1000.0)).is_err(), "损坏 PNG 应返回 Err 而非 panic");
        let dir = tmp_dir("png_ok");
        let p = dir.join("ok.png");
        write_png(&p, 4, 4);
        let ok = std::fs::read(&p).unwrap();
        assert!(decode_background(&ok, (1000.0, 1000.0)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：打开 20MP **PNG** → 首解即「显示尺寸档」1/2（2560×1920，不糊），缓存只存
    /// 这份清晰档（镜像 JPEG 版 jpeg_cold_open_installs_sharp_display）。
    #[gpui::test]
    async fn png_cold_open_installs_sharp_display(cx: &mut TestAppContext) {
        let dir = tmp_dir("png_sharp");
        let p = dir.join("big.png");
        write_png(&p, 5120, 3840);
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
        assert_eq!(den, 2, "PNG 首解应为 1/2 显示尺寸档，而非全解 1/1：{den}");
        assert_eq!((w, h), (2560, 1920));
        assert_eq!(cache_len, 1, "缓存应是唯一一份清晰档");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PNG 升档：放大使展示像素需要超过 1/2 档时按需重解 1/1 全解替换（镜像 JPEG 版
    /// zoom_in_escalates_display_to_full_resolution；升档门已去掉 `path_is_jpeg` 限定）。
    #[gpui::test]
    async fn png_zoom_escalates_display_to_full_resolution(cx: &mut TestAppContext) {
        let dir = tmp_dir("png_esc");
        let p = dir.join("big.png");
        write_png(&p, 5120, 3840);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        assert_eq!(cx.update(|app| viewer.read(app).display_denom), 2, "首解 1/2");
        // 放大 ≥2×：需要 (2752,2064) > 1/2(2560×1920) → 应升到 1/1 全解
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
        assert_eq!(d1, 1, "PNG 放大 2× 后应升档到 1/1 全解，而非停在 1/2");
        assert_eq!((w1, h1), (5120, 3840));
        assert_eq!(cached, vec![(1, 5120, 3840)], "缓存应升级为唯一全解档");
        // 再次触发（无需升档）应为空操作：不新增解码、档位不变
        cx.update(|app| viewer.update(app, |v, cx| v.escalate_display_denom(cx)));
        let stable = cx.update(|app| viewer.read(app).display_denom);
        assert_eq!(stable, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PNG 已满足区的升档**空操作**回归：den=2 输出 D=(2560×1920) 已 ≥ 展示需要
    /// (1440×1080)（1080p 视口内 4:3 图、zoom≈1）时，升档判定不得再触发重解——选档基准
    /// 必须按全量原图估回（×display_denom），否则 desired=1<2 每触必重解却落回同一分母。
    #[gpui::test]
    async fn png_escalate_noop_when_display_denom_already_satisfies(cx: &mut TestAppContext) {
        let dir = tmp_dir("png_noop");
        let p = dir.join("big.png");
        write_png(&p, 5120, 3840);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        assert_eq!(cx.update(|app| viewer.read(app).display_denom), 2, "首解 1/2");
        // 缩放前（need 被 1/2 档覆盖）：升档应为空操作——不新增任何解码
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.display_need = (1440.0, 1080.0);
                v.escalate_display_denom(cx);
            })
        });
        let (den, in_flight, cache_len) = cx.update(|app| {
            let v = viewer.read(app);
            (v.display_denom, v.in_flight.len(), v.cache.len())
        });
        assert_eq!(den, 2, "满足区升档不得改变档位");
        assert_eq!(in_flight, 0, "满足区升档不得新增解码（否则每次缩放/回看都白解一次 9MP PNG）");
        assert_eq!(cache_len, 1, "缓存不得被无谓重解替换");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降采样开销占比测定（`--ignored --nocapture` 运行）：9MP（3000×3000）PNG 冷解码墙钟
    /// vs 其中盒式降采样步骤耗时（串行参考 vs rayon 行组并行）。用于论证降采样并行取舍：
    /// 若占比可观（本机实测 ~35%+），rayon 行组并行值得上；数值随铁反映。
    #[test]
    #[ignore]
    fn bench_png_downsample_fraction() {
        let dir = tmp_dir("ds_frac");
        let p = dir.join("big.png");
        write_pattern_png(&p, 3000, 3000);
        let bytes = std::fs::read(&p).unwrap();
        let need = (1376.0, 1032.0);
        // 独立复刻 decode 的「全量解码」步骤，只计时「降采样」两版本
        let dec = kimg::codecs::png::PngDecoder::new(Cursor::new(&bytes)).unwrap();
        let (iw, ih) = dec.dimensions();
        let color = dec.color_type();
        let n = usize::try_from(dec.total_bytes()).unwrap();
        let mut raw = vec![0u8; n];
        dec.read_image(&mut raw).unwrap();
        let rgba = if color == kimg::ColorType::Rgba8 {
            raw
        } else {
            expand_png_to_rgba8(&raw, iw, ih, color).unwrap()
        };
        let t0 = std::time::Instant::now();
        let _ = box_downsample_bgra_or_rgba(&rgba, iw, ih, 2);
        let ms_ds_ser = t0.elapsed().as_secs_f64() * 1000.0;
        let t0 = std::time::Instant::now();
        let _ = box_downsample_bgra_or_rgba_rayon(&rgba, iw, ih, 2);
        let ms_ds_par = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let _ = decode_png_display(&bytes, need).unwrap();
        let ms_total = t1.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "\n[PNG-DS] {}×{}: 全链路 {:.1}ms\n  降采样(den=2) 串行 {:.1}ms ({:.1}%) | rayon 并行 {:.1}ms (并行收益 {:.1}×)\n  → 生产走 rayon：降占比 {:.1}%",
            iw, ih, ms_total, ms_ds_ser, ms_ds_ser / ms_total.max(0.001) * 100.0, ms_ds_par,
            ms_ds_ser / ms_ds_par.max(0.001), ms_ds_par / ms_total.max(0.001) * 100.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_missing_file_reports_error() {
        let p = tmp_dir("missing").join("没有的文件.png");
        assert!(decode_image_at(&p).is_err());
    }

    /// 真实 HEIF/HEIC（手机 48-tile grid）应能经查看器解码路径 (`decode_generic` →
    /// `decode_static_first_frame` → `kimg::load_from_memory`) 解出 BGRA 帧；缺失语料 → SKIP。
    /// 回归：扩展名清单曾缺 heic/heif 导致 .HEIC 被当文本打开失败。
    #[test]
    fn heic_decodes_via_viewer_path_to_bgra() {
        let corpus =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif/IMG_0083.HEIC");
        let bytes = match std::fs::read(corpus) {
            Ok(b) => b,
            Err(e) => {
                println!("SKIP: missing {corpus} ({e})");
                return;
            }
        };
        let (frames, w, h) = decode_generic(&bytes).expect("真实 HEIC 应能解码");
        assert_eq!((w, h), (3024, 4032), "IMG_0083 显示尺寸（irot 后竖图）");
        assert_eq!(frames.len(), 1, "静态 HEIC 单帧");
        let buf = frames[0].buffer();
        let len = w as usize * h as usize;
        assert_eq!(buf.len(), len * 4, "BGRA 字节数");
        assert_eq!(buf[3], 255, "alpha 通道应为 255（BGRA 第 4 字节）");
        // 解码背景档：HEIC 走展示档降档（IMG_0083 3024×4032，需要 1024 → 1/2 = 1512×2016 ≥ 需要）
        let (frames2, w2, h2, den) = decode_background(&bytes, (1024.0, 1024.0))
            .expect("decode_background 应能解 HEIC");
        assert!((w2, h2) == (1512, 2016), "HEIC 展示档应降到 1/2，输出 {w2}×{h2}");
        assert_eq!(den, 2, "HEIC 展示档应降档（非全解）");
        assert_eq!(frames2[0].buffer().len(), w2 as usize * h2 as usize * 4);
    }

    /// HEIF 展示档**降档**回归：need 远小于原图 → den≥2，输出宽高 = ceil(全量/den) 且
    /// ≥ display need（对齐 [`pick_den`] 的「输出 ≥ 显示需要」语义，同
    /// `pick_den_never_below_display_pixels`）。用最轻合成语料（1600×900，debug 快）。
    #[test]
    fn heif_display_denoms_when_need_small() {
        let corpus = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../Benchmarks/corpus/heif/synth-photo-fuji-1600x900-main8.heic"
        );
        let bytes = match std::fs::read(corpus) {
            Ok(b) => b,
            Err(e) => {
                println!("SKIP: missing {corpus} ({e})");
                return;
            }
        };
        let (frames, w, h, den) = decode_background(&bytes, (200.0, 150.0)).expect("HEIF 降档应成功");
        assert!(den >= 2, "need 远小于原图应降档，den={den}");
        let (full, wf, hf) = decode_generic(&bytes).expect("真实 HEIF 应能全解");
        assert_eq!(full.len(), 1, "静态 HEIF 单帧");
        assert_eq!(w, wf.div_ceil(u32::from(den)), "输出宽 = ceil(全量/den)");
        assert_eq!(h, hf.div_ceil(u32::from(den)), "输出高 = ceil(全量/den)");
        // R5A：contain 实际显示尺寸（按图纵横比把 need 折算）——解码 ≥ 显示才不糊
        let eff = (
            200.0f32.min(150.0 * wf as f32 / hf as f32),
            150.0f32.min(200.0 * hf as f32 / wf as f32),
        );
        assert!(
            (w as f32) >= eff.0 && (h as f32) >= eff.1,
            "解码不得低于 contain 显示尺寸（不糊）：{w}×{h} vs 显示 {:.1}×{:.1}",
            eff.0,
            eff.1
        );
        assert_eq!(frames[0].buffer().len(), (w as usize * h as usize * 4) as usize);
    }

    /// HEIF 展示档 **den=1 字节恒等**：need 超原图 → 与改动前 `decode_generic`（
    /// `decode_static_first_frame` → `kimg::load_from_memory`）全解**逐像素一致**（RGBA→
    /// BGRA 交换后）。守护降档/交换顺序不破坏既有全解路径。
    #[test]
    fn heif_display_den1_matches_full_decode_bytes() {
        let corpus = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../Benchmarks/corpus/heif/synth-photo-fuji-1600x900-main8.heic"
        );
        let bytes = match std::fs::read(corpus) {
            Ok(b) => b,
            Err(e) => {
                println!("SKIP: missing {corpus} ({e})");
                return;
            }
        };
        let (full, wf, hf) = decode_generic(&bytes).expect("全解应成功");
        let (disp, wd, hd, den) = decode_background(&bytes, (99999.0, 99999.0)).expect("全解降档应成功");
        assert_eq!(den, 1, "need 超原图应落全解");
        assert_eq!((wd, hd), (wf, hf));
        assert_eq!(
            disp[0].buffer().as_raw(),
            full[0].buffer().as_raw(),
            "den=1 输出必须与改动前全解逐像素一致"
        );
    }

    /// 损坏 HEIF 字节（ftyp head 合法但无实际 HEVC 数据）→ `decode_background` 返回
    /// `Err` 而非 panic（HEIF 展示档失败 → 回退全解仍失败 → 顶层以加载错误呈现，同 PNG/JPEG
    /// 兜底姿势）。
    #[test]
    fn corrupt_heif_bytes_return_err_not_panic() {
        let bad: &[u8] = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic";
        let r = decode_background(bad, (100.0, 100.0));
        assert!(r.is_err(), "损坏 HEIF 应返回 Err 而非 panic");
    }

    /// HEIF 识别（`is_heic_magic`/`is_heavy_format`）：`ftyp` 所列 brand 含 HEIF 系 → 是；
    /// AVIF brand（常连同 mif1 列出但不是 HEVC）→ 否（kimg `heif_brands` 语义）；其它格式
    /// 不误判。
    #[test]
    fn heic_magic_and_heavy_format_detect_ftyp_brands_only() {
        let heic: &[u8] = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic";
        assert!(is_heic_magic(heic), "major brand heic 应识别");
        assert!(is_heavy_format(heic), "HEIF 即重型格式");
        // AVIF：avif/avis brand 不算 HEIF（kimg 与 image/C# CanDecode 同步）
        let avif: &[u8] = b"\x00\x00\x00\x1Cftypavif\x00\x00\x00\x00mif1avif";
        assert!(!is_heic_magic(avif), "AVIF 不是 HEIF brand，不应进入 HEIF 展示档分支");
        // 其它格式不误判
        assert!(!is_heic_magic(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]), "PNG 非 HEIF");
        assert!(!is_heic_magic(&[0xFF, 0xD8, 0xFF, 0xE0]), "JPEG 非 HEIF");
        assert!(!is_heic_magic(b"GIF89a"), "GIF 非 HEIF");
    }

    /// 端到端：打开真实 HEIF → 首解即「展示尺寸档」（need 压小 → den≥2，不糊），且缓存
    /// 只存这份降档；放大使需要超全量 → 走 [`Self::escalate_display_denom`]（已去 jpeg 限定）
    /// 升档全解替换（镜像 JPEG/PNG 版 `*_cold_open_installs_sharp_display`）。合成语料最轻，
    /// debug 快；缺语料 → SKIP。
    #[gpui::test]
    async fn heif_cold_open_installs_sharp_display_and_escalates(cx: &mut TestAppContext) {
        let corpus = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../Benchmarks/corpus/heif/synth-photo-fuji-1600x900-main8.heic"
        );
        let p = std::path::PathBuf::from(corpus);
        if !p.exists() {
            println!("SKIP: missing {corpus}");
            return;
        }
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        // 后台扫描未完成（首解未派发）前把展示需要压小 → 首解即降档（用最轻语料验证 den 链）
        cx.update(|app| {
            viewer.update(app, |v, _| v.display_need = (200.0, 150.0));
        });
        wait_until_ready(cx, &viewer);
        let (den, w, h) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.display_denom,
                v.intrinsic.map(|i| i.0 as u32).unwrap_or(0),
                v.intrinsic.map(|i| i.1 as u32).unwrap_or(0),
            )
        });
        assert!(den >= 2, "HEIF 首解应为展示尺寸档而非全解：den={den}");
        // R5A：contain 实际显示尺寸（1600×900 对角 (200,150) need → 约 200×112.5）——
        // 解码 ≥ 显示才不糊；短轴不再要求 ≥ 视口原始 need。
        let eff = (
            200.0f32.min(150.0 * w as f32 / h as f32),
            150.0f32.min(200.0 * h as f32 / w as f32),
        );
        assert!(
            (w as f32) >= eff.0 && (h as f32) >= eff.1,
            "输出不得低于 contain 显示尺寸（不糊）：{w}×{h} vs 显示 {:.1}×{:.1}",
            eff.0,
            eff.1
        );
        // 放大：需要超全量 → 升档 1/1 全解
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.display_need = (2000.0, 1500.0);
                v.escalate_display_denom(cx);
            })
        });
        flush(cx);
        let (d1, w1, h1) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.display_denom,
                v.intrinsic.map(|i| i.0 as u32).unwrap_or(0),
                v.intrinsic.map(|i| i.1 as u32).unwrap_or(0),
            )
        });
        assert_eq!(d1, 1, "放大超全量应升档 1/1 全解，而非停在降档");
        let full_bytes = std::fs::read(&p).unwrap();
        let (_, fw, fh) = decode_generic(&full_bytes).expect("全解基准应成功");
        assert_eq!((w1, h1), (fw, fh), "升档后应为全解尺寸");
    }

    /// 重型并发闸状态机：模拟 HEAVY_DECODE_MAX 路 HEIF 在途 → ①重型目标进
    /// `pending_target` 等空位（不派发、不计数）②重型预取直接放弃（不改 pending_target）
    /// ③非重型 PNG 不受重型闸 → 立即派发 ④空位放出（重型计数自减）→ `drain_pending`
    /// 重入队即放行。用最轻合成语料副本；缺语料 → SKIP。
    #[gpui::test]
    async fn heif_heavy_cap_gates_target_abandons_prefetch_and_lets_light_through(
        cx: &mut TestAppContext,
    ) {
        const HEAVY: usize = config::image::HEAVY_DECODE_MAX;
        let dir = tmp_dir("heif_cap");
        let synth = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../Benchmarks/corpus/heif/synth-photo-fuji-1600x900-main8.heic"
        );
        if !std::path::Path::new(synth).exists() {
            println!("SKIP: missing {synth}");
            return;
        }
        let (a, b, c) = (dir.join("a.heic"), dir.join("b.heic"), dir.join("c.heic"));
        std::fs::copy(synth, &a).unwrap();
        std::fs::copy(synth, &b).unwrap();
        std::fs::copy(synth, &c).unwrap();
        let png = dir.join("light.png");
        write_png(&png, 64, 64);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(a.clone(), cx)));
        // 模拟 HEAVY_DECODE_MAX 路 HEIF 已在途：重型并发满
        cx.update(|app| viewer.update(app, |v, _| v.heavy_inflight = HEAVY));
        // ① 重型**目标** → 进 pending_target 等空位，不派发、重型计数不增
        cx.update(|app| viewer.update(app, |v, cx| v.queue_decode(b.clone(), true, cx)));
        let (pending, b_on, heavy) = cx.update(|app| {
            let v = viewer.read(app);
            (v.pending_target.clone(), v.in_flight.contains(&b), v.heavy_inflight)
        });
        assert_eq!(pending.as_deref(), Some(b.as_path()), "重型目标应记 pending_target");
        assert!(!b_on, "重型目标在并发满时不得立即派发");
        assert_eq!(heavy, HEAVY, "被闸拒绝的请求不得计入重型在途");
        // ② 重型**预取** → 直接放弃：pending_target 不变、不派发
        cx.update(|app| viewer.update(app, |v, cx| v.queue_decode(c.clone(), false, cx)));
        let (pending2, c_on) = cx.update(|app| {
            let v = viewer.read(app);
            (v.pending_target.clone(), v.in_flight.contains(&c))
        });
        assert_eq!(pending2.as_deref(), Some(b.as_path()), "重型预取放弃不得改 pending_target");
        assert!(!c_on, "重型预取应直接放弃");
        // ③ 非重型（PNG 预取）不受重型闸 → 立即派发，也不参与重型计数
        cx.update(|app| viewer.update(app, |v, cx| v.queue_decode(png.clone(), false, cx)));
        let (png_on, heavy2) = cx.update(|app| {
            let v = viewer.read(app);
            (v.in_flight.contains(&png), v.heavy_inflight)
        });
        assert!(png_on, "PNG 不受重型并发闸限制（MAX_DECODE=8 语义保持）");
        assert_eq!(heavy2, HEAVY, "PNG 不参与重型计数");
        // ④ 空位放出（重型计数 2→1）→ drain_pending 重入队：重型目标应放行派发
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.heavy_inflight = HEAVY - 1;
                v.drain_pending(cx);
            })
        });
        let (b_on2,) = cx.update(|app| {
            let v = viewer.read(app);
            (v.in_flight.contains(&b),)
        });
        assert!(b_on2, "空位放出后重型目标应放行派发");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 编码一张 N 帧测试 GIF（每帧统一 4×3 逻辑屏全画布；(0,0) 像素为 `color`、帧 delay 为
    /// `ms`）。用 `GifEncoder`（image 0.25 默认特性含 gif 编码）。GIF 编码 delay 精度
    /// 10ms（写盘时除 10 取整），测试参数须用 10 的倍数才能精确往返。
    fn encode_test_gif(frames: &[(kimg::Rgba<u8>, u32)]) -> Vec<u8> {
        let (w, h) = (4u32, 3u32);
        let mut out = Vec::new();
        let mut enc = kimg::codecs::gif::GifEncoder::new(&mut out);
        for (color, ms) in frames {
            let mut buf = kimg::RgbaImage::from_pixel(w, h, kimg::Rgba([0, 0, 0, 0]));
            buf.put_pixel(0, 0, *color);
            let frame = Frame::from_parts(
                buf,
                0,
                0,
                kimg::Delay::from_numer_denom_ms(*ms, 1),
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
            (kimg::Rgba([255, 0, 0, 255]), 100),
            (kimg::Rgba([0, 0, 255, 255]), 200),
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
        let bytes = encode_test_gif(&[(kimg::Rgba([10, 20, 30, 255]), 0)]);
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
            frames.push((kimg::Rgba([(i % 256) as u8, 0, 0, 255]), 10));
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
            (kimg::Rgba([10, 20, 30, 255]), 120),
            (kimg::Rgba([200, 100, 50, 255]), 120),
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

    /// 写一张有内容（渐变）的 JPEG：编码走测试模块内自包含的**最小 baseline JPEG 编码器**
    /// （[`encode_baseline_jpeg`]：DC-only + 标准 Huffman/量化表，YCbCr 4:4:4 不下采样，
    /// 零依赖）——`image`(=kimg) 无 JPEG 编码器、其 save 只支持 PNG/GIF（原 turbo 参照
    /// 通道已移除）。渐变前 8 像素起纯黑（首 8×8 块全 0）→ 解码角点精确
    /// (0,0,0)，供降档「角点黑 / 右下渐变」断言稳定。
    fn write_jpeg(path: &Path, w: u32, h: u32) {
        let mut img = kimg::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // 前 8 像素裁为纯黑（首 8×8 块全 0），其余照旧渐变
            let (rx, ry) = (x.saturating_sub(8), y.saturating_sub(8));
            let r = ((rx * 255) / w) as u8;
            let g = ((ry * 255) / h) as u8;
            let b = ((rx + ry) * 60 / (w + h)) as u8;
            *px = kimg::Rgb([r, g, b]);
        }
        let jpeg = encode_baseline_jpeg(img.as_raw(), w, h);
        std::fs::write(path, jpeg).expect("写出渐变 JPEG 应成功");
    }

    /// 标准 Annex K 量化表（zigzag 存储序）。本编码器只用 DC 项（索引 0），AC 项仅供
    /// 解码例行读取；luma DC=16 能整除 1024（纯黑块 DC 精确 -64，解码角点恰为 0）。
    static LUM_Q: [u8; 64] = [
        16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49,
        35, 37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56,
        80, 109, 81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103,
        99,
    ];
    static CHR_Q: [u8; 64] = [
        17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99,
        99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ];

    /// 标准 Annex K Huffman 表：bits[i] = 码长 (i+1) 的码字个数，随后 values 为对应符号。
    static DC_LUM_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
    static DC_LUM_VAL: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    static DC_CHR_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
    static DC_CHR_VAL: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    static AC_LUM_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
    static AC_LUM_VAL: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    static AC_CHR_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
    static AC_CHR_VAL: [u8; 162] = [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];

    /// 最小 baseline JPEG 编码器（零依赖，测试夹具专用）：把行优先 R/G/B 交错的 `w×h`
    /// 栅格编码成标准 baseline JPEG 字节流。**只写 DC 系数**（每 8×8 块一 DC + AC 表 EOB，
    /// AC 恒 0）——这是合法 baseline JPEG 的平凡子集（场景为平坦块渐变图），量化/重构后
    /// 即「块内均值」，标准 K 表 + 4:4:4 不下采样下任何 baseline 解码器（含自研 kimg_jpeg）
    /// 都能解。**O(像素)** 纯遍历：5120×3840 20MP 亦即时完成，无堆栈大缓冲。
    fn encode_baseline_jpeg(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
        let dc_lum_q = i64::from(LUM_Q[0]);
        let dc_chr_q = i64::from(CHR_Q[0]);

        /// Huffman 表：按符号值索引，`code`/`len` 为码字与码长。
        #[derive(Clone, Copy)]
        struct Hf {
            code: u16,
            len: u8,
        }
        fn build_ht(bits: &[u8; 16], vals: &[u8]) -> [Hf; 256] {
            let mut t = [Hf { code: 0, len: 0 }; 256];
            let mut code: u32 = 0;
            let mut k = 0usize;
            for len in 1..=16u32 {
                for _ in 0..bits[(len - 1) as usize] {
                    let v = vals[k] as usize;
                    t[v] = Hf {
                        code: code as u16,
                        len: len as u8,
                    };
                    code += 1;
                    k += 1;
                }
                code <<= 1;
            }
            t
        }

        /// MSB 优先位写器：字节填充（0xFF 后补 0x00），末尾不足字节补 1。
        struct BitWriter {
            buf: Vec<u8>,
            acc: u32,
            nbits: u32,
        }
        impl BitWriter {
            fn new() -> Self {
                Self { buf: Vec::with_capacity(4096), acc: 0, nbits: 0 }
            }
            fn put(&mut self, val: u32, n: u32) {
                self.acc = (self.acc << n) | val;
                self.nbits += n;
                while self.nbits >= 8 {
                    self.nbits -= 8;
                    let b = ((self.acc >> self.nbits) & 0xFF) as u8;
                    self.buf.push(b);
                    if b == 0xFF {
                        self.buf.push(0); // 字节填充
                    }
                }
            }
            fn finish(mut self) -> Vec<u8> {
                if self.nbits > 0 {
                    let pad = 8 - self.nbits;
                    let b = ((self.acc << pad) | ((1u32 << pad) - 1)) & 0xFF;
                    self.buf.push(b as u8);
                }
                self.buf
            }
        }

        /// 写单个 DC 差分系数：符号 = 幅值位数，负数用一补形态。
        fn put_dc(bw: &mut BitWriter, tbl: &[Hf; 256], diff: i32) {
            let size = if diff == 0 {
                0
            } else {
                32 - diff.unsigned_abs().leading_zeros()
            };
            let h = tbl[size as usize];
            bw.put(u32::from(h.code), u32::from(h.len));
            if size > 0 {
                // 幅值：正数 = 原值；负数 = 2^size - 1 - |值|（JPEG 一补形态，MSB=0）
                let mag = if diff >= 0 {
                    diff as u32
                } else {
                    (1u32 << size) - 1 - diff.unsigned_abs()
                };
                bw.put(mag, size);
            }
        }

        /// 组装「标记 + 长度 + 载荷」段。
        fn push_seg(out: &mut Vec<u8>, marker: u8, payload: &[u8]) {
            out.push(0xFF);
            out.push(marker);
            out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(payload);
        }

        let dc_lum = build_ht(&DC_LUM_BITS, &DC_LUM_VAL);
        let ac_lum = build_ht(&AC_LUM_BITS, &AC_LUM_VAL);
        let dc_chr = build_ht(&DC_CHR_BITS, &DC_CHR_VAL);
        let ac_chr = build_ht(&AC_CHR_BITS, &AC_CHR_VAL);

        let mcu_x = (w + 7) / 8;
        let mcu_y = (h + 7) / 8;
        // 四舍五入除法（半值远离零）：DC = round(Σ/8 / Q)；对纯黑块（Σ=-8192, luma Q=16）
        // 恰整除 -64，解码角点精确为 0。
        let rdiv = |sum: i64, q: i64| -> i32 {
            let half = q / 2;
            ((sum + if sum >= 0 { half } else { -half }) / q) as i32
        };
        // YCbCr→DCT 域 DC（含右/下边缘复制到 64 采样）：BT.601 full-range
        let (mut py, mut pc, mut pr) = (0i32, 0i32, 0i32);
        let mut bw = BitWriter::new();
        for my in 0..mcu_y {
            for mx in 0..mcu_x {
                let (mut sy, mut sc, mut sr) = (0i64, 0i64, 0i64);
                for yy in 0..8u32 {
                    let y = (my * 8 + yy).min(h - 1);
                    let row = &rgb[((y as usize) * (w as usize)) * 3..];
                    for xx in 0..8u32 {
                        let x = (mx * 8 + xx).min(w - 1);
                        let o = x as usize * 3;
                        let (r, g, b) = (row[o], row[o + 1], row[o + 2]);
                        let yv = (299 * i32::from(r) + 587 * i32::from(g) + 114 * i32::from(b)) / 1000;
                        let cb = 128
                            + (-168736 * i32::from(r) - 331264 * i32::from(g) + 500000 * i32::from(b))
                                / 1000000;
                        let cr = 128
                            + (500000 * i32::from(r) - 418688 * i32::from(g) - 81312 * i32::from(b))
                                / 1000000;
                        sy += i64::from(yv - 128);
                        sc += i64::from(cb - 128);
                        sr += i64::from(cr - 128);
                    }
                }
                let y_dc = rdiv(sy, 8 * dc_lum_q);
                let cb_dc = rdiv(sc, 8 * dc_chr_q);
                let cr_dc = rdiv(sr, 8 * dc_chr_q);
                put_dc(&mut bw, &dc_lum, y_dc - py);
                py = y_dc;
                bw.put(u32::from(ac_lum[0].code), u32::from(ac_lum[0].len)); // EOB
                put_dc(&mut bw, &dc_chr, cb_dc - pc);
                pc = cb_dc;
                bw.put(u32::from(ac_chr[0].code), u32::from(ac_chr[0].len)); // EOB
                put_dc(&mut bw, &dc_chr, cr_dc - pr);
                pr = cr_dc;
                bw.put(u32::from(ac_chr[0].code), u32::from(ac_chr[0].len)); // EOB
            }
        }

        let mut out = Vec::with_capacity(bw.buf.capacity());
        out.extend_from_slice(&[0xFF, 0xD8]); // SOI
        // APP0 JFIF 1.01（无缩略）
        push_seg(
            &mut out,
            0xE0,
            &[b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00],
        );
        // DQT：亮度 0 + 色度 1
        let mut dqt = Vec::with_capacity(130);
        dqt.push(0x00);
        dqt.extend_from_slice(&LUM_Q);
        dqt.push(0x01);
        dqt.extend_from_slice(&CHR_Q);
        push_seg(&mut out, 0xDB, &dqt);
        // SOF0：baseline、精度 8、3 分量 4:4:4（采样 0x11）
        let mut sof = Vec::with_capacity(17);
        sof.push(8);
        sof.extend_from_slice(&(h as u16).to_be_bytes());
        sof.extend_from_slice(&(w as u16).to_be_bytes());
        sof.push(3);
        sof.extend_from_slice(&[1, 0x11, 0]); // Y
        sof.extend_from_slice(&[2, 0x11, 1]); // Cb
        sof.extend_from_slice(&[3, 0x11, 1]); // Cr
        push_seg(&mut out, 0xC0, &sof);
        // DHT：DC/AC × 亮度/色度 四表
        let mut dht = Vec::new();
        dht.push(0x00);
        dht.extend_from_slice(&DC_LUM_BITS);
        dht.extend_from_slice(&DC_LUM_VAL);
        dht.push(0x10);
        dht.extend_from_slice(&AC_LUM_BITS);
        dht.extend_from_slice(&AC_LUM_VAL);
        dht.push(0x01);
        dht.extend_from_slice(&DC_CHR_BITS);
        dht.extend_from_slice(&DC_CHR_VAL);
        dht.push(0x11);
        dht.extend_from_slice(&AC_CHR_BITS);
        dht.extend_from_slice(&AC_CHR_VAL);
        push_seg(&mut out, 0xC4, &dht);
        // SOS：3 分量的 DC/AC 表选择 + 谱选择（baseline 固定 0,63,0）
        let sos = [3u8, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0];
        push_seg(&mut out, 0xDA, &sos);
        out.extend_from_slice(&bw.finish());
        out.extend_from_slice(&[0xFF, 0xD9]); // EOI
        out
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

    /// K124 安全反例：SOF 声明 16-bit 上限 65535×65535 的小文件——`try_decode_jpeg_scaled`
    /// 必须在**分配前**因「scaled 输出 > 512MiB 硬上界」返回 `None`，不得做 GB 级分配
    /// （受 `DECODE_DISPLAY_MAX_BYTES` 拦下；`set_limits(default)` 使 codec 侧
    /// `read_image_scaled` 的 `max_alloc` reserve 同口径生效）。
    #[test]
    fn try_decode_jpeg_scaled_rejects_huge_header_without_alloc() {
        // 本模块最小 baseline JPEG 编码器产 64×64，再把 SOF0 宽高 patch 成 65535×65535
        let mut jpeg = encode_baseline_jpeg(&[0u8; 64 * 64 * 3], 64, 64);
        patch_jpeg_sof_dims(&mut jpeg, 65535, 65535).expect("自产 JPEG 应含 SOF0 可 patch");
        // 视口 (30000,30000) → pick_den 落 den=2（65535 宽 → scaled 32768 ≥ need）
        let mut dec = kimg::codecs::jpeg::JpegDecoder::from_bytes(&jpeg).expect("头解析应成功");
        assert_eq!(dec.dimensions(), (65535, 65535), "patch 后声明 65535×65535");
        assert_eq!(
            dec.scaled_dimensions(2),
            Some((65535u32.div_ceil(2), 65535u32.div_ceil(2)))
        );
        // 3.0GB > 512MiB → 分配前拒绝（None → 上层走全解兜底）
        assert!(
            try_decode_jpeg_scaled(&jpeg, (30000.0, 30000.0)).is_none(),
            "超大 SOF 必须在分配前拒绝"
        );
    }

    /// 手写「IHDR 声明超大尺寸」的最小 PNG（P1-27 夹具）：幅面伪造为 `w×h`，IDAT 给足
    /// `idat_len` 字节（零填充）——kimg 解析期「防炸弹预检」按 `压缩量×1032` 是否盖得住
    /// 声明像素字节数判断（chunk.rs `MAX_INFLATE_RATIO`），故 IDAT 长度不足会直接在 `new()`
    /// 被拒、根本走不到预算预检。要构造「能到 `total_bytes`/`vec!` 段、但总字节超预算」的真
    /// 炸弹夹具，IDAT 必须 ≥ 声明像素字节/1032（K173：测试要命中真防御路径）。
    fn oversized_declared_png(w: u32, h: u32, idat_len: usize) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8bit RGBA
        out.extend_from_slice(&png_chunk_byte(b"IHDR", &ihdr));
        out.extend_from_slice(&png_chunk_byte(b"IDAT", &vec![0u8; idat_len]));
        out.extend_from_slice(&png_chunk_byte(b"IEND", &[]));
        out
    }

    /// K124 安全反例（PNG 侧，P1-27）：IHDR 声明 11600×11600 RGBA（≈513MiB，**刚好超过**
    /// 512MiB 分配预算上限）的小文件——若把预算预检去掉，`vec![0u8; total]` 会先做出
    /// >512MiB 的单次分配（1GB 级备选尺寸更糟），再等解码限流。断言 [`decode_png_display`]
    /// 在分配前因「`total_bytes` > 硬上界」返回 `Err`，且错误文案钉死「预算」（排除解码器
    /// 其它随机错误，K173）。
    #[test]
    fn decode_png_display_rejects_oversized_ihdr_without_alloc() {
        // 11600×11600×4 ≈ 513.3MiB > 512MiB；IDAT 给足 560KB（≥ 513MiB/1032）以通过 kimg
        // 解析期防炸弹预检 → 真能走到 decode_png_display 的 total_bytes / vec! 段。
        let png = oversized_declared_png(11600, 11600, 560_000);
        let dec = kimg::codecs::png::PngDecoder::new(Cursor::new(&png)).expect("头应可解析");
        assert_eq!(dec.dimensions(), (11600, 11600), "声明尺寸应被解析");
        assert!(
            dec.total_bytes() > DECODE_DISPLAY_MAX_BYTES,
            "夹具 total_bytes 必须超过预算（{} > {})",
            dec.total_bytes(),
            DECODE_DISPLAY_MAX_BYTES
        );
        // K173：拒绝必须来自「预算预检」——文案钉死「预算」，且发生在 vec! 分配之前
        let err = decode_png_display(&png, DISPLAY_NEED_INIT)
            .expect_err("超大声明尺寸应被预算预检拒绝，而非触发达分配");
        assert!(err.contains("预算"), "拒绝文案应指明预算预检：{err}");
    }

    /// 预算判定纯函数（P1-27/K124）：≤512MiB 放行、超限拒绝；65535×65535×4 RGBA ≈17GB
    /// 必须拒绝。单测直接钉预检逻辑（decode 级集成测试见上，K173 防假绿）。
    #[test]
    fn png_decode_within_budget_rejects_oversized_pixel_bytes() {
        assert!(png_decode_within_budget(DECODE_DISPLAY_MAX_BYTES), "预算边界内应放行");
        assert!(
            !png_decode_within_budget(65535u64 * 65535 * 4),
            "65535×65535×4 ≈17GB 必须拒绝"
        );
        assert!(png_decode_within_budget(1), "极小图放行");
    }

    /// 定位 JPEG SOF0（`FF C0`）并原地 patch 高/宽（段布局 `FF C0 <len:2> <prec:1> <h:2> <w:2>`）。
    fn patch_jpeg_sof_dims(bytes: &mut Vec<u8>, w: u16, h: u16) -> Option<()> {
        for i in 0..bytes.len().saturating_sub(8) {
            if bytes[i] == 0xFF && bytes[i + 1] == 0xC0 {
                bytes[i + 5..i + 7].copy_from_slice(&h.to_be_bytes());
                bytes[i + 7..i + 9].copy_from_slice(&w.to_be_bytes());
                return Some(());
            }
        }
        None
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
        // 一档 = 1 + ZOOM_STEP；再缩一档 = 1/(1+ZOOM_STEP)（精确逆比，往返闭合回到 1.0——
        // 旧 (1+s)(1-s)=1-s²≈0.9856 会让页宽在反复放大缩小时持续缩水、回不到原值）。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
                assert!((v.zoom - 1.12).abs() < 1e-6, "1 档放大: {}", v.zoom);
                v.adjust_zoom(pan_zoom::zoom_factor(-1.0, ZOOM_STEP), cx);
                assert!((v.zoom - 1.0).abs() < 1e-4, "再缩一档回原位: {}", v.zoom);
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

    /// P2-① 唯一性：缩放真实生效后 `scroll_accum` 残留清零（切滚轮语义，防放大态残留
    /// 立即误触发一次平移/切图）；缩放未生效（已钳满，`apply_zoom` 拒绝）时不动。
    #[gpui::test]
    async fn adjust_zoom_clears_scroll_accum_residue(cx: &mut TestAppContext) {
        let dir = tmp_dir("zoom_accum");
        // 图片无需真实存在（display_denom==1 时 escalate 早退，不派解码）。
        let viewer: gpui::Entity<ImageViewer> = cx.update(|app| {
            app.new(|cx| {
                let mut v = ImageViewer::new(dir.join("a.jpg"), cx);
                v.intrinsic = Some((200.0, 100.0));
                v.last_viewport = (800.0, 600.0);
                v.scroll_accum = 12.5; // 残留亚步长
                v.zoom = ZOOM_MAX; // 已钳满：再放大不生效 → 不应清零
                v
            })
        });
        // zoom 已满 → apply_zoom 拒绝 → 残留保留。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.adjust_zoom(pan_zoom::zoom_factor(1.0, ZOOM_STEP), cx);
            })
        });
        assert_eq!(
            cx.update(|app| viewer.read(app).scroll_accum),
            12.5,
            "缩放未生效（已钳满）不应清零残留"
        );
        // 缩一档真实生效 → 清零。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.adjust_zoom(pan_zoom::zoom_factor(-1.0, ZOOM_STEP), cx);
            })
        });
        assert_eq!(
            cx.update(|app| viewer.read(app).scroll_accum),
            0.0,
            "缩放真实生效后 scroll_accum 残留应清零"
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

    /// P1-28 滚轮方向一致性：同一物理滚轮（向下滚 = delta.y<0 → 下一张，向上滚 → 上一张）
    /// 在 `Lines`（鼠标）与 `Pixels`（触控板）两种模式必须切换方向一致——旧实现 Pixels
    /// 分支 `+= delta_y` 让向下滚变「上一张」，与 Lines 相反（同机两外设方向打架）。走真实
    /// `on_scroll_wheel` 事件分发（覆盖 `scroll_accum` 累计路径；依赖窗口上下文取事件）。
    #[gpui::test]
    async fn wheel_lines_and_pixels_switch_same_direction(cx: &mut TestAppContext) {
        use gpui::{point, px};
        let dir = tmp_dir("wheel_dir");
        for n in ["a.png", "b.png", "c.png"] {
            write_png(&dir.join(n), 8, 8);
        }
        let (viewer, wcx) = cx.add_window_view(|_window, cx| {
            let mut v = ImageViewer::new(dir.join("a.png"), cx);
            v.last_viewport = (800.0, 600.0);
            v.intrinsic = Some((100.0, 50.0)); // 1× 适配视口，未放大 → 直接切图
            v.zoom = 1.0;
            v
        });
        let vcx: &mut VisualTestContext = wcx;
        vcx.run_until_parked(); // 等后台扫描落定后统一覆盖 nav，避免竞态
        vcx.update(|window, cx| {
            viewer.update(cx, |v, c| {
                v.nav = FolderNav::new(
                    vec![dir.join("a.png"), dir.join("b.png"), dir.join("c.png")],
                    0,
                );
                // 行滚轮（鼠标）：向下滚（delta.y<0）→ 下一张（基线语义）
                v.on_scroll_wheel(
                    &gpui::ScrollWheelEvent {
                        position: point(px(200.), px(200.)),
                        delta: gpui::ScrollDelta::Lines(point(0.0f32, -1.0f32)),
                        modifiers: gpui::Modifiers::default(),
                        ..Default::default()
                    },
                    window,
                    c,
                );
                assert_eq!(v.nav.current(), 1, "行滚轮向下滚应切下一张");
                // 像素滚轮（触控板）：同一物理向下滚（delta.y<0）→ 必须同样切下一张
                v.on_scroll_wheel(
                    &gpui::ScrollWheelEvent {
                        position: point(px(200.), px(200.)),
                        delta: gpui::ScrollDelta::Pixels(point(
                            px(0.),
                            px(-config::image::WHEEL_PIXEL_STEP),
                        )),
                        modifiers: gpui::Modifiers::default(),
                        ..Default::default()
                    },
                    window,
                    c,
                );
                assert_eq!(
                    v.nav.current(),
                    2,
                    "像素滚轮向下滚应切下一张（与行滚轮同方向）"
                );
            });
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
        // B≈60,G≈254,R≈254 → BGRA [B,G,R,255]（自研 kimg 盒式降档 BGRA 直出 = GPUI 字节序）。
        assert_eq!([bgra[0], bgra[1], bgra[2], bgra[3]], [0, 0, 0, 255]);
        let tail = &bgra[(bgra.len() - 4)..];
        assert!(tail[0] < 150 && tail[1] > 200 && tail[2] > 200, "右下应低蓝高红：{tail:?}");
        // 展示需要 ≥ 原图 → 全解（1/1）
        let (_b, w2, _h2, den2) = decode_background(&bytes, (400.0, 400.0)).unwrap();
        assert_eq!((w2, den2), (400, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_den_never_below_display_pixels() {
        // 20MP + 1080p 视口需要 (1376,1032)：1/2（2560×1920）恰好全cover、1/4（1280×960）
        // 会低于 1376 → 必须选 1/2 —— 这就是「不糊」的边界。
        assert_eq!(pick_den(5120, 3840, 1376.0, 1032.0), 2);
        assert_eq!(pick_den(5120, 3840, 1280.0, 960.0), 4);
        // 4:3 图在 (1281,960) 盒内 contain 实际显示为 1280×960（高度贴边、宽度按比例投影）：
        // 1/4 档 1280×960 ≥ 显示尺寸 → 仍不糊（R5A 按纵横比折算后不再「宽度超一像素就升 1/2」）。
        assert_eq!(pick_den(5120, 3840, 1281.0, 960.0), 4, "contain 折算后 1/4 档恰好盖住显示");
        // 缩小看（zoom<1）→ 需要变小 → 可压到 1/8（640×480 ≥ 275×206）
        assert_eq!(pick_den(5120, 3840, 275.0, 206.0), 8);
        // 放大 2× → 需要 (2752,2064) > 1/2（2560×1920）→ 全解
        assert_eq!(pick_den(5120, 3840, 2752.0, 2064.0), 1);
        // 小图不需要降档
        assert_eq!(pick_den(800, 600, 1920.0, 1080.0), 1);
        // R5A：竖图（2552×3496）在 1080p/横视口下 —— 显示宽度永远到不了视口宽（高度先
        // 贴边），按纵横比折算后短轴目标 ≈788、不再卡死 den=1 → 1/2（1276×1748 ≥ 显示）；
        // 视口更小（600×800）可压到 1/4（638×874 ≥ 显示）。横图（4000×3000 @1376×1032）
        // 回归不变（仍 1/2）。
        assert_eq!(pick_den(2552, 3496, 1920.0, 1080.0), 2, "竖图 1080p 视口应 1/2 不再 1");
        assert_eq!(pick_den(2552, 3496, 1376.0, 1032.0), 2);
        assert_eq!(pick_den(2552, 3496, 600.0, 800.0), 4);
        // 放大 2× → 需要折算 (1506,2064) > 1/2（1276×1748）→ 全解（竖图 get 全分辨率）
        assert_eq!(pick_den(2552, 3496, 2752.0, 2064.0), 1);
        assert_eq!(pick_den(4000, 3000, 1376.0, 1032.0), 2, "横图回归不变");
    }

    /// R5B：通用静态（BMP 3ch）**展示档降档**——竖图不再卡 den=1、横图回归不变、den=1 与
    /// 旧静态全解**逐字节一致**（字节 = 显示尺寸、RGBA→BGRA 命中角点色）。
    #[test]
    fn bmp_display_downscales_vertical_keeps_landscape_and_den1_parity() {
        let dir = tmp_dir("bmp_disp");
        let grad = |w: u32, h: u32| {
            move |x: u32, y: u32| [((x * 255) / w) as u8, ((y * 255) / h) as u8, 0u8]
        };
        // 竖图 2552×3496 + 1080p 需 need → 1/2 档 1276×1748（不再 1）；像素数 < 全尺寸
        let vpath = dir.join("vert.bmp");
        write_bmp(&vpath, 2552, 3496, grad(2552, 3496));
        let vb = std::fs::read(&vpath).unwrap();
        let (vf, vw, vh, vden) = decode_background(&vb, (1920.0, 1080.0)).expect("竖图 BMP 降档应成功");
        assert_eq!((vw, vh, vden), (1276, 1748, 2), "竖图 BMP 应 1/2 档");
        assert_eq!(vf[0].buffer().as_raw().len(), (1276 * 1748 * 4) as usize);
        assert!((vw as u64) * (vh as u64) < (2552u64 * 3496), "展示档像素必须 < 全尺寸");
        // 左上角 (0,0) 源 RGB(0,0,0) → BGRA [0,0,0,255]
        assert_eq!(&vf[0].buffer().as_raw()[..4], &[0, 0, 0, 255]);
        // 横图 4000×3000 + 1376×1032 需 need → 仍 1/2 档（回归不变）
        let hpath = dir.join("land.bmp");
        write_bmp(&hpath, 4000, 3000, grad(4000, 3000));
        let hb = std::fs::read(&hpath).unwrap();
        let (_, hw, hh, hden) = decode_background(&hb, (1376.0, 1032.0)).expect("横图 BMP 降档应成功");
        assert_eq!((hw, hh, hden), (2000, 1500, 2), "横图 BMP 回归不变");
        // den=1 全解 parity：小 BMP + 超大 need → 与旧静态全解逐字节一致
        let spath = dir.join("small.bmp");
        write_bmp(&spath, 64, 48, grad(64, 48));
        let sb = std::fs::read(&spath).unwrap();
        let (sf, sw, sh, sden) = decode_background(&sb, (1e6, 1e6)).expect("小 BMP 应全解");
        assert_eq!((sw, sh, sden), (64, 48, 1));
        let (oldf, _, _) = decode_generic(&sb).expect("旧静态全解应成功");
        assert_eq!(
            sf[0].buffer().as_raw(),
            oldf[0].buffer().as_raw(),
            "den=1 display 与旧静态全解必须逐字节一致"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5B：TIFF 静态（手写 RGB8，kimg_tiff 3ch）展示档降档 → 1/2、字节=显示尺寸。
    #[test]
    fn tiff_display_downscales_large_static() {
        let dir = tmp_dir("tiff_disp");
        let p = dir.join("big.tiff");
        write_rgb8_tiff(&p, 2048, 1536, |x, y| [((x * 255) / 2048) as u8, ((y * 255) / 1536) as u8, 30u8]);
        let b = std::fs::read(&p).unwrap();
        let (frames, w, h, den) = decode_background(&b, (1000.0, 750.0)).expect("TIFF 降档应成功");
        assert_eq!((w, h, den), (1024, 768, 2), "TIFF 应 1/2 档");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].buffer().as_raw().len(), (1024 * 768 * 4) as usize);
        // 左上角 (0,0) 源 RGB(0,0,30) → BGRA [30,0,0,255]
        assert_eq!(&frames[0].buffer().as_raw()[..4], &[30, 0, 0, 255]);
        // den=1（need ≥ 原图）→ 全尺寸、字节=显示
        let (_, w1, h1, den1) = decode_background(&b, (4096.0, 4096.0)).unwrap();
        assert_eq!((w1, h1, den1), (2048, 1536, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5B：GIF 动画保持全尺寸 den=1（动画帧必须全量驻留才能播；范围墙不做动画降档）——
    /// decode_background 对 GIF 走回全帧动图路径、帧数/逻辑屏尺寸不变。
    #[test]
    fn gif_anim_keeps_den1_and_full_frames() {
        let bytes = encode_test_gif(&[
            (kimg::Rgba([255, 0, 0, 255]), 100),
            (kimg::Rgba([0, 0, 255, 255]), 200),
        ]);
        let (frames, w, h, den) = decode_background(&bytes, (1920.0, 1080.0)).expect("GIF 应解全帧动图");
        assert_eq!((w, h, den), (4, 3, 1), "GIF 动画必须 den=1 全尺寸");
        assert_eq!(frames.len(), 2, "动画原帧数不变");
    }

    /// R5B 端到端：冷开一张 8.9MP 竖图 BMP → 首解即「显示尺寸档」（1/2，1276×1748），缓存
    /// 条目尺寸 = display < 全尺寸、字节 ≈ 8.9MB（不再 35.7MB 全解）。测试执行器无视口 →
    /// 用 [`DISPLAY_NEED_INIT`]（≈1080p）作展示需要。
    #[gpui::test]
    async fn bmp_cold_open_installs_display_entry_sub_full(cx: &mut TestAppContext) {
        let dir = tmp_dir("bmp_open");
        let p = dir.join("big.bmp");
        write_bmp(&p, 2552, 3496, |x, y| [((x * 255) / 2552) as u8, ((y * 255) / 3496) as u8, 0]);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        let (den, iw, ih, bytes, cache_len) = cx.update(|app| {
            viewer.update(app, |v, _| {
                let e = v.cache_get(&p).expect("已安装条目应在缓存");
                (e.denom, e.intrinsic.0 as u32, e.intrinsic.1 as u32, e.bytes, v.cache.len())
            })
        });
        assert_eq!((den, iw, ih), (2, 1276, 1748), "缓存条目=显示档 1/2");
        assert_eq!(bytes, (1276 * 1748 * 4) as usize, "条目字节=显示尺寸");
        assert!(bytes < (2552 * 3496 * 4) as usize, "条目必须 < 全尺寸");
        assert!(cache_len >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5B 端到端：目录内多张 8.9MP BMP 全解码后，缓存总字节 ≤ 显示档总和（≈4×8.9MB），
    /// 远低于 4×全解 35.7MB——不再「每张 34MB 纹理 + 26×34MB」级。
    #[gpui::test]
    async fn bmp_dir_all_display_entries_bound_memory(cx: &mut TestAppContext) {
        let dir = tmp_dir("bmp_mem");
        let (w, h) = (2552u32, 3496u32);
        let mut paths = Vec::new();
        for i in 0..4 {
            let p = dir.join(format!("{i}.bmp"));
            write_bmp(&p, w, h, |x, y| [((x * 255) / w) as u8, ((y * 255) / h) as u8, 0]);
            paths.push(p);
        }
        let img_w = (w / 2) as usize * (h / 2) as usize * 4; // 每张 display 档字节（den=2）
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(paths[0].clone(), cx)));
        wait_until_ready(cx, &viewer);
        // 把其余 3 张逐一入队解码（预取也会落缓存）→ 落定后核对缓存总字节
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                for p in &paths[1..] {
                    v.queue_decode(p.clone(), false, cx);
                }
            })
        });
        wait_until_ready(cx, &viewer);
        let (cache_bytes, full_sum) = cx.update(|app| {
            let v = viewer.read(app);
            (v.cache.bytes(), paths.len() * (w as usize * h as usize * 4))
        });
        assert!(
            cache_bytes <= paths.len() * img_w,
            "缓存总字节应 ≤ 显示档总和：{cache_bytes} ≤ {}",
            paths.len() * img_w
        );
        assert!(cache_bytes < full_sum, "缓存总字节应远低于全解总和 {full_sum}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5B WEBP 展示档降档（真实语料，release/本地验证用）：本地 `Benchmarks/corpus/WEBP`
    /// 缺夹具置 SKIP，绝不视为回归。`--ignored --nocapture` 运行。
    #[test]
    #[ignore]
    fn webp_display_downscales_from_corpus() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/WEBP");
        let cands = ["gen32-photo-large-lossless.webp", "gen27-photo-rgba-lossless.webp"];
        let Some(p) = cands
            .iter()
            .map(|n| std::path::Path::new(dir).join(n))
            .find(|p| p.exists())
        else {
            eprintln!("SKIP: missing {dir}");
            return;
        };
        let b = std::fs::read(&p).unwrap();
        let need = (1376.0, 1032.0);
        let (frames, w, h, den) = decode_background(&b, need).expect("WEBP 降档应成功");
        eprintln!("WEBP {}: full→display {}×{} den=1/{den}", p.display(), w, h);
        assert!(den > 1, "大 WEBP 应走降档，den={den}");
        assert!((w as u64) * (h as u64) < (2000u64 * 2000), "展示档应明显小于全尺寸");
        assert_eq!(frames.len(), 1);
    }

    /// R5B 目录内存基准（真实语料，26 张 BMP）：全解 RGBA 总和 vs 展示档总和。
    /// 本地 `Benchmarks/corpus/BMP` 缺夹具置 SKIP。`--ignored --nocapture` 运行。
    #[test]
    #[ignore]
    fn bmp_26_corpus_display_memory() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/BMP");
        if !std::path::Path::new(dir).exists() {
            eprintln!("SKIP: missing {dir}");
            return;
        }
        let need = (1920.0, 1080.0);
        let mut full_px: u64 = 0;
        let mut disp_px: u64 = 0;
        let mut n = 0u32;
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for p in files {
            let b = std::fs::read(&p).unwrap();
            let (_, wf, hf) = decode_generic(&b).expect("BMP 全解应成功");
            let (_, wd, hd, den) = decode_background(&b, need).expect("BMP 展示档应成功");
            full_px += wf as u64 * hf as u64;
            disp_px += wd as u64 * hd as u64;
            n += 1;
            eprintln!("  {} {}×{} → display {}×{} (1/{})", p.file_name().unwrap().to_string_lossy(), wf, hf, wd, hd, den);
        }
        eprintln!(
            "[BMP-MEM] {n} 张 | 全解 RGBA {:.1}MB → 展示档 {:.1}MB（{:.0}× 降）",
            full_px as f64 * 4.0 / 1e6,
            disp_px as f64 * 4.0 / 1e6,
            (full_px as f64) / (disp_px as f64)
        );
        assert!(disp_px < full_px, "展示档总像素必须 < 全解总像素");
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
                !v.state.loading,
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
    /// 自研 kimg 全解 + 盒式降档直出 BGRA）**。展示需要固定为 1080p 视口 contain 的真实量
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
            "\n[IMG-BENCH] {} 张 | 全解(旧 image) vs 展示尺寸(JPEG/PNG kimg 全解+盒式降采样, 需要 {:.0}×{:.0})：",
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
        let mut img = kimg::RgbaImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 255) / w) as u8;
            let g = ((y * 255) / h) as u8;
            let b = ((x + y) * 60 / (w + h)) as u8;
            *px = kimg::Rgba([r, g, b, 255]);
        }
        img.save(path).expect("写出渐变 PNG 应成功");
    }

    /// 把测试执行器里的异步任务（含后台解码/预读链）全部排空。
    fn flush(cx: &mut TestAppContext) {
        for _ in 0..8 {
            cx.run_until_parked();
        }
    }

    /// 反复泵到后台解码/字节预读**全部落定**（在途集合清空）：后台解码在真实线程池上跑，
    /// `run_until_parked` 只处理已排队的完成回调、不等待其真实完成——本循环持续泵到
    /// `in_flight`/`byte_flight` 清空，把异步余量收敛为确定性状态（R2 快滚闸确定性断言用）。
    fn wait_idle(cx: &mut TestAppContext, viewer: &gpui::Entity<ImageViewer>) {
        let mut guard = 0;
        loop {
            cx.run_until_parked();
            let idle = cx.update(|app| {
                let v = viewer.read(app);
                v.in_flight.is_empty() && v.byte_flight.is_empty()
            });
            if idle {
                return;
            }
            guard += 1;
            assert!(guard < 20000, "等待后台解码/字节预读落定超时");
        }
    }

    /// 等到「当前显示目标已就绪」（loading==false 且已非首帧）：缓存命中时首次刻就绪
    /// （≈0）；缓存未中时轮询执行器直到后台解码落盘。带安全上限防死循环。
    fn wait_until_ready(cx: &mut TestAppContext, viewer: &gpui::Entity<ImageViewer>) {
        let mut guard = 0;
        loop {
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                !v.state.loading && v.state.data.is_some()
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

    /// B 战役锚表母线：corpus 真实目录「快速连续切换 30 格」压力——模拟用户快速甩滚轮
    /// 逐张切图。语义：等首图就绪 + 排空预取后，**先进入快速模式**（连续快滚到
    /// [`config::image::FAST_SCROLL_THRESHOLD`] 并排空「进模式」首 1-2 格尚未触发节流时的
    /// 预取洪泛），再**批量提交** 30 次 `switch(1)`（事件洪峰，与 `bench_switch_pipeline`
    /// B4 段落同款姿势；注意 headless 测试里 `run_until_parked` 会连后台解码一起排空，若
    /// 逐格泵会退化成「每格等一张解完」——故用批量提交建模洪峰）：
    /// ① 30 次提交总墙钟（事件到达成本；测试执行器内同步微秒级，主要看追尾）；
    /// ② 提交完 → 最终目标就绪的**追尾延迟**（用户感知「滚到头才出图」）；
    /// ③ 「解码派发 N」——进入段 flush 已把前向窗口预取进缓存，批量 30 格目标基本暖命中，
    ///    该计数实为「缓存未中（窗口外）尾数」而非解码池利用率；对 `in_flight` 集合逐采样
    ///    取并集（既有字段、零生产改动；快解码可能在采样间隙完成而低估）；
    /// ④ 在途/预取/缓存字节峰值（解码队列堆积的定性观察）；
    /// ⑤ 反向回退 5 格（落出快速前向窗口回看，LRU 淘汰后回看成本）每格 ready 延迟。
    /// 目录用 `IMG_SWITCH_DIR` env（真实 corpus）；未设则生成 40 张渐变 PNG 兜底。
    /// `--ignored --nocapture` 运行。
    #[gpui::test]
    #[ignore]
    async fn bench_switch_rapid_corpus(cx: &mut TestAppContext) {
        use std::collections::HashSet;
        use std::time::Instant;
        const RAPID: usize = 30;
        // 快滚节流闸上限（bench 内复引用，便于断言与打印）
        const MAX_FAST: usize = config::image::FAST_SCROLL_MAX_DECODE;

        // 目录：env `IMG_SWITCH_DIR`（corpus 真目录）→ 未设时生成渐变 PNG 兜底
        let (dir, generated) = match std::env::var("IMG_SWITCH_DIR").ok() {
            Some(d) => (PathBuf::from(d), false),
            None => {
                let d = tmp_dir("rapid");
                for i in 0..40u32 {
                    write_pattern_png(&d.join(format!("{:02}.png", i)), 1200, 900);
                }
                (d, true)
            }
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("读取目录失败 {}: {e}", dir.display()))
            .flatten()
            .map(|e| e.path())
            .filter(|p| crate::is_image_extension(p))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "目录内无图片可测：{}", dir.display());
        assert!(
            files.len() >= RAPID + 6,
            "样本不足（需 ≥{}+6 张才能跑 30 格+回退 5）：{}",
            RAPID,
            files.len()
        );
        let first = files[0].clone();
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(first, cx)));
        // 首图就绪（含后台文件夹扫描 + 首图解码）→ 排空预取链（窗口铺满，模拟停留后甩滚轮）
        wait_until_ready(cx, &viewer);
        flush(cx);
        let (cache0, bytes0, nav0, byte_b0) = cx.update(|app| {
            let v = viewer.read(app);
            (v.cache.len(), v.cache.bytes(), v.nav.len(), v.byte_cache.bytes())
        });
        assert!(nav0 > 1, "文件夹扫描应建立兄弟清单：nav.len()={nav0}");

        // 事件洪峰：批量提交 30 次 switch(1)，中间不泵（后台解码在真实线程池并行跑，完成回调
        // 排队到前台；快滚节流下并发满 2 即进 pending_target 追最新）
        let mut dispatched: HashSet<PathBuf> = HashSet::new();
        let mut peak_inflight = 0usize;
        let mut peak_prefetch = 0usize;
        let mut peak_cache = cache0;
        let mut peak_cache_bytes = bytes0;
        let mut peak_byte_cache_bytes = byte_b0;

        // 先进入快速模式并排空「进模式」的首 1-2 格（scroll_fast 尚未触发时的 8 路预取洪泛）
        // ——真实用户「停留→甩滚轮」同样先沉淀再快滚，本段只建模进入稳态、不计入洪峰测量
        for _ in 0..config::image::FAST_SCROLL_THRESHOLD + 1 {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        flush(cx);
        let fast_armed = cx.update(|app| viewer.read(app).scroll_fast);
        assert!(fast_armed, "连续快滚应已进入快速模式（scroll_fast）");

        let t0 = Instant::now();
        for _ in 0..RAPID {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        let submit_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // 洪峰刚落：首批并发派发同步可见（追最新目标在 pending_target 等空位）
        cx.update(|app| {
            let v = viewer.read(app);
            for p in &v.in_flight {
                dispatched.insert(p.clone());
            }
            peak_inflight = peak_inflight.max(v.in_flight.len());
            peak_prefetch = peak_prefetch.max(v.prefetch_inflight);
        });

        // 追尾：提交流回 → 泵到最终目标就绪，采样续派发解码与队列峰值
        let t1 = Instant::now();
        let mut guard = 0;
        loop {
            cx.run_until_parked();
            let ready = cx.update(|app| {
                let v = viewer.read(app);
                for p in &v.in_flight {
                    dispatched.insert(p.clone());
                }
                peak_inflight = peak_inflight.max(v.in_flight.len());
                peak_prefetch = peak_prefetch.max(v.prefetch_inflight);
                peak_cache = peak_cache.max(v.cache.len());
                peak_cache_bytes = peak_cache_bytes.max(v.cache.bytes());
                peak_byte_cache_bytes = peak_byte_cache_bytes.max(v.byte_cache.bytes());
                v.is_loaded()
            });
            if ready {
                break;
            }
            guard += 1;
            assert!(guard < 2000, "等待快速切换目标就绪超时");
        }
        let tail_ms = t1.elapsed().as_secs_f64() * 1000.0;
        flush(cx); // 排空残余预取再读终态
        // 快滚节流：本段 30 格洪峰全程处于快速模式（scroll_fast 已在进入段置位），在途并发
        // 峰值必须收敛到 FAST_SCROLL_MAX_DECODE（动态上限按构造成立，断言防回归）
        assert!(
            peak_inflight <= MAX_FAST,
            "快滚节流下在途并发峰值应 ≤ FAST_SCROLL_MAX_DECODE={MAX_FAST}，实为 {peak_inflight}"
        );
        let (cache1, bytes1, scroll_fast, idx_now) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.cache.len(),
                v.cache.bytes(),
                v.scroll_fast,
                v.nav.current(),
            )
        });

        // 反向回退 5 格：每格等 ready（第 5 格通常落出快速前向窗口 → 真实重解码成本）
        let mut backs: Vec<f64> = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            cx.update(|app| viewer.update(app, |v, cx| v.switch(-1, cx)));
            wait_until_ready(cx, &viewer);
            backs.push(t.elapsed().as_secs_f64() * 1000.0);
            flush(cx);
        }

        eprintln!(
            "\n[IMG-BENCH] rapid-switch: dir={} ({:>3} 张) 批量 {RAPID} 格提交 {:7.2}ms · 追尾(最后目标就绪) {:7.1}ms · 解码派发 {} 个 · 在途峰值 {} (快滚闸≤{MAX_FAST}) / 预取峰值 {} · 缓存 {}→{} 条目 {:>6.0}MB→{:>6.0}MB · 字节缓存峰值 {:>6.0}MB · 滚动后下标 {idx_now} scroll_fast={scroll_fast} · 回退5格 ready {:?}ms",
            dir.display(),
            files.len(),
            submit_ms,
            tail_ms,
            dispatched.len(),
            peak_inflight,
            peak_prefetch,
            cache0,
            cache1,
            bytes0 as f64 / 1048576.0,
            bytes1 as f64 / 1048576.0,
            peak_byte_cache_bytes as f64 / 1048576.0,
            backs
        );
        eprintln!(
            "[IMG-BENCH] rapid-switch detail: 快滚节流（scroll_fast）在途并发峰值 ≤FAST_SCROLL_MAX_DECODE={MAX_FAST}; 解码派发 {} 个（去重路径，含追最新重解）; 追尾段=目标未命中→pending/重解→落盘; 回退 5 格延迟 min {:6.2}ms / max {:6.2}ms（第 5 格 {:6.2}ms 通常已落出快速前向窗口）",
            dispatched.len(),
            backs.iter().cloned().fold(f64::MAX, f64::min),
            backs.iter().cloned().fold(f64::MIN, f64::max),
            backs.last().copied().unwrap_or(0.0)
        );
        if generated {
        let _ = std::fs::remove_dir_all(&dir);
    }
    }

    /// R2 快滚节流闸 · 确定性单测：快速模式下解码并发必须收敛到
    /// [`config::image::FAST_SCROLL_MAX_DECODE`]，且并发满后最新目标不被丢（chase-latest
    /// 生效）。三个点检，均不依赖后台完成时序：
    /// ①（端到端）进入快速模式后连续快速切图：在途并发 ≤ 上限，且末位目标不丢（已记
    ///    pending / 已追入在途 / 已装屏）；pump 后最新目标被追上并上屏（红于 chase-latest
    ///    失效：满时不再记 pending / drain 不再放出 / 追不上目标）。
    /// ②（确定性满池门控，红于 `max_decode` 被改坏）人为把在途填到上限，直接 `queue_decode`
    ///    冷目标 → 不得再派生（记 pending），在途保持 = 上限；`max_decode()` 不再收敛回
    ///    [`config::image::FAST_SCROLL_MAX_DECODE`] 时满池判定用错上限 → 继续派生 → 红。
    #[gpui::test]
    async fn fast_scroll_throttles_inflight_and_chases_latest(cx: &mut TestAppContext) {
        let dir = tmp_dir("fast_throttle");
        let n = 40u32;
        for i in 0..n {
            write_pattern_png(&dir.join(format!("{:02}.png", i)), 1200, 900);
        }
        let first = dir.join("00.png");
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(first, cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer); // 开窗预取全落定：确定性起点（in_flight=0、窗口暖缓存）
        // 进入快速模式（连续快滚到阈值）并落定（进模式只派发少量预取，全部收敛为 0 在途）
        for _ in 0..config::image::FAST_SCROLL_THRESHOLD + 1 {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        wait_idle(cx, &viewer);
        let fast = cx.update(|app| viewer.read(app).scroll_fast);
        assert!(fast, "连续快滚应已触发快速模式（scroll_fast）");

        // 无 pump 批量快速切图（连续同向 → scroll_fast 全程保持、不触发迟滞衰减）：目标逐格
        // 前进，末位目标（3+STEPS=29，n=40）稳落出缓存窗口（冷）→ 在途只受快滚闸约束
        // （满则记 pending 追最新）。同步 switch 之间不显式泵，但本执行器也会处理已排队的
        // 后台完成回调，故不依赖「零在途完成」时序假设。
        const STEPS: usize = 26;
        for _ in 0..STEPS {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        // 点检 1：在途并发 ≤ 快滚闸（派生受 `max_decode()` 门控 → 按构造成立；改坏上限即红）。
        // 注：chase-latest 的「满时记 pending」可能已被 drain 提前把最新追入在途/装屏（本测试
        // 执行器在同步 switch 之间也会处理后台完成回调），故点检 2 判据为「最新目标不丢」：
        // 已在 pending / 已追入在途 / 已装屏（install）三者之一即算覆盖。
        let (inflight, latest_covered, latest_path) = cx.update(|app| {
            let v = viewer.read(app);
            let latest = v
                .nav
                .item(v.nav.current())
                .expect("switch 目标恒在清单内");
            (
                v.in_flight.len(),
                v.pending_target.as_deref() == Some(latest.as_path())
                    || v.in_flight.contains(&latest)
                    || v.path == latest,
                latest,
            )
        });
        assert!(
            inflight <= config::image::FAST_SCROLL_MAX_DECODE,
            "快滚节流闸：快速切图在途并发应 ≤ FAST_SCROLL_MAX_DECODE={}，实为 {inflight}",
            config::image::FAST_SCROLL_MAX_DECODE
        );
        assert!(
            latest_covered,
            "快滚并发满时最新目标不得丢失：应已记 pending_target / 已追入在途 / 已装屏（chase-latest）"
        );

        // 点检 2：pump → chase-latest 追上最新目标并上屏（追尾兜底：丢目标则 wait 超时红）
        wait_until_ready(cx, &viewer);
        let (loaded, installed) = cx.update(|app| {
            let v = viewer.read(app);
            (v.is_loaded(), v.path.clone())
        });
        assert!(loaded, "pump 后最新目标应就绪（chase-latest 生效）");
        assert_eq!(
            installed.file_name(),
            latest_path.file_name(),
            "应安装最新目标 {latest_path:?}，实为 {installed:?}"
        );

        // 点检 3（确定性「满并发」门控，红于 max_decode 被改坏）：人为把解码池填到
        // FAST_SCROLL_MAX_DECODE 个在途（远端未解码路径），直接 queue_decode 冷目标 →
        // 不再派生（满则记 pending_target 追最新），in_flight 保持 = FAST_SCROLL_MAX_DECODE
        // 不增长。「fast 态不再收敛」（max_decode 恒回 MAX_DECODE）时满池判定用错上限 →
        // 新目标被继续派生 → 断言红。不用 switch（无导航/迟滞副作用），target 为不存在的
        // 远端路径（若错了被派生也只走失败分支，不会入缓存/上屏）。
        let gate_target = dir.join("__gate__0.png");
        let gate_d1 = dir.join("__gate__1.png");
        let gate_d2 = dir.join("__gate__2.png");
        cx.update(|app| {
            viewer.update(app, |v, _| {
                v.in_flight.insert(gate_d1);
                v.in_flight.insert(gate_d2);
            })
        });
        cx.update(|app| viewer.update(app, |v, cx| v.queue_decode(gate_target.clone(), true, cx)));
        let (gate_inflight, gate_pending) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.in_flight.len(),
                v.pending_target == Some(gate_target.clone()),
            )
        });
        assert_eq!(
            gate_inflight,
            config::image::FAST_SCROLL_MAX_DECODE,
            "快滚满并发时不得再派生新解码：in_flight 应为 {}，实为 {gate_inflight}（max_decode 被改坏？）",
            config::image::FAST_SCROLL_MAX_DECODE
        );
        assert!(
            gate_pending,
            "快滚满并发时新目标应记入 pending_target（chase-latest）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4A 重绘收口：快速连滚切到**未就绪**目标时——不发同步 notify（不整窗重绘）、
    /// 显示保持上一张已安装图（不置空/不抢换），换图只由目标落盘（`install_current`）
    /// 通知；30 格未就绪切换的总 notify 次数必须 << 30。用 `App::observe` 精确计数实体
    /// 的 notify（外层 `cx.update` 收尾即 flush effects，批量中同步 notify 可逐格读到）。
    #[gpui::test]
    async fn fast_scroll_unready_switch_defers_repaint_keeps_display(cx: &mut TestAppContext) {
        let dir = tmp_dir("r4a_defer");
        const N: u32 = 40;
        for i in 0..N {
            write_pattern_png(&dir.join(format!("{:02}.png", i)), 600, 400);
        }
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(dir.join("00.png"), cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer); // 预取窗口铺满、在途清零 → 确定性起点

        let prev = cx.update(|app| viewer.read(app).state.data.clone());
        assert!(prev.is_some(), "首图应已安装");

        // 清空解码缓存 → 后续 30 次切图全部未就绪（无缓存命中、无同步 install）
        cx.update(|app| {
            viewer.update(app, |v, _| v.cache.clear());
        });

        // observe 计数器（实体每 notify 一次 +1）
        let notifies = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let _sub = {
            let n = notifies.clone();
            cx.update(|app| app.observe(&viewer, move |_, _| n.set(n.get() + 1)))
        };

        // 事件洪峰：批量 30 次 switch(1)，中间不 pump（与 bench_switch_rapid_corpus 同款）
        for _ in 0..30 {
            cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        }
        // ① 未就绪切图不得同步 notify（否则每滚一格就触发一次整窗重绘）
        let during = notifies.get();
        assert_eq!(
            during,
            0,
            "30 个未就绪切图不得产生任何同步 notify（不整窗重绘）：during={during}"
        );
        // ② 显示保持上一张已安装图（image Arc 指针不变 = 未置空/未抢换）
        let mid = cx.update(|app| viewer.read(app).state.data.clone());
        assert!(
            Arc::ptr_eq(prev.as_ref().unwrap(), mid.as_ref().unwrap()),
            "未就绪期间必须保持上一张已安装图像（不得置空/抢换）"
        );
        // ③ 落定后：最终目标换图（落盘 notify），总次数 << 30、且最终显示最新目标
        wait_until_ready(cx, &viewer);
        let total = notifies.get();
        assert!(
            total > 0,
            "目标落盘后应至少 notify 一次触发最终换图：total={total}"
        );
        assert!(
            total < 30,
            "30 格未就绪切图的总 notify 次数应 << 30（只由落盘/缓存命中触发）：{total}"
        );
        let (loaded, file) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.is_loaded(),
                v.nav.item(v.nav.current()).map(|p| p.to_path_buf()),
            )
        });
        assert!(loaded, "最终目标应就绪");
        assert_eq!(file, Some(dir.join("30.png")), "最终应显示最新目标 30.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4A 消除闪烁（JPEG 带 EXIF 嵌入缩略图）：切到**未就绪 JPEG** 时不得再派 EXIF
    /// 缩略图占位抢换显示——`thumb_pending` 不被置位（同步可断言）＝ 不会闪出模糊小图、
    /// 也不会因占位落盘多触发一次重绘。冷打开（无上一张）仍走占位，由既有
    /// `jpeg_cold_open_exif_thumb_then_sharp` 守护。
    #[gpui::test]
    async fn switch_to_unready_jpeg_skips_exif_thumb_provisional(cx: &mut TestAppContext) {
        let dir = tmp_dir("r4a_jpeg");
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        write_jpeg(&assets.join("thumb.jpg"), 160, 120);
        for name in ["00.jpg", "01.jpg"] {
            write_jpeg(&dir.join(name), 800, 600);
            let main = std::fs::read(dir.join(name)).unwrap();
            let thumb = std::fs::read(assets.join("thumb.jpg")).unwrap();
            std::fs::write(
                dir.join(name),
                wrap_exif_thumb(main.as_slice(), thumb.as_slice()),
            )
            .unwrap();
        }
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(dir.join("00.jpg"), cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer); // 01 可能已被预取入缓存 → 落定后清空，转为未就绪

        cx.update(|app| {
            viewer.update(app, |v, _| v.cache.clear());
        });
        let prev = cx.update(|app| viewer.read(app).state.data.clone());
        assert!(prev.is_some());

        // 切到 01（未就绪 JPEG）→ 同步断言：不派 EXIF 缩略图占位
        cx.update(|app| viewer.update(app, |v, cx| v.switch(1, cx)));
        let (pending, kept, path_ok) = cx.update(|app| {
            let v = viewer.read(app);
            (
                v.thumb_pending.is_none(),
                Arc::ptr_eq(prev.as_ref().unwrap(), v.state.data.as_ref().unwrap()),
                v.path == dir.join("01.jpg"),
            )
        });
        assert!(path_ok, "切图后 path 应指向 01.jpg");
        assert!(
            pending,
            "未就绪 JPEG 切换不得派 EXIF 缩略图占位（会闪缩略图 + 多一次重绘）"
        );
        assert!(kept, "切换未就绪 JPEG 期间应保持上一张已安装图（不得被缩略图抢换）");

        // 落定：换到 01 真图（headless 无视口 → display_need 回落 (1,1) → 走展示降档
        // den>1；EXIF 缩略图占位恒 den=1、160×120——den != 1 即证明最终是「真图降档解码」
        // 而非缩略图兜底），thumb_pending 保持清空。
        wait_until_ready(cx, &viewer);
        let (den, thumb_gone, loaded) = cx.update(|app| {
            let v = viewer.read(app);
            (v.display_denom, v.thumb_pending.is_none(), v.is_loaded())
        });
        assert!(loaded, "01 落定后应就绪");
        assert!(
            den != 1,
            "最终应显示 01 真图降档解码（缩略图占位恒 den=1），实为 den={den}"
        );
        assert!(thumb_gone, "真图落盘后 thumb_pending 应清空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-30：`release()` 必须自停动图时钟并释放动图片段引用——旧实现只 drop 上屏/缓存
    /// 纹理，不动 `anim_gen`/`anim_clip`/`anim_index`：release 后旧动画任务下个 tick 把帧
    /// 重新写回 `state.data`、且 `anim_clip` 持有的全部帧 Arc 不释放 → VRAM 不回收。断言
    /// release 后 `anim_gen` 递增（在飞时钟自停）、`anim_clip` 清空、`anim_index` 归零。
    #[gpui::test]
    async fn release_stops_anim_clock_and_clears_clip(cx: &mut TestAppContext) {
        let dir = tmp_dir("animrel");
        write_png(&dir.join("a.png"), 4, 4);
        let p = dir.join("a.png");
        let mk = |c: [u8; 3]| -> Arc<RenderImage> {
            Arc::new(RenderImage::new(vec![kimg::Frame::new(kimg::RgbaImage::from_pixel(
                2,
                2,
                kimg::Rgba([c[0], c[1], c[2], 255]),
            ))]))
        };
        let f0 = mk([1, 2, 3]);
        let f1 = mk([4, 5, 6]);
        let (viewer, wcx) = cx.add_window_view(|_window, cx| ImageViewer::new(p.clone(), cx));
        let vcx: &mut VisualTestContext = wcx;
        vcx.run_until_parked(); // 完成初始首帧解码（静态 PNG → anim_clip=None 基线）
        // 挂起动图状态（模拟 GIF 播放中）
        vcx.update(|_, cx| {
            viewer.update(cx, |v, _c| {
                v.state.data = Some(f0.clone());
                v.anim_clip = Some((
                    vec![f0.clone(), f1],
                    vec![
                        std::time::Duration::from_millis(50),
                        std::time::Duration::from_millis(50),
                    ],
                ));
                v.anim_gen = 7;
                v.anim_index = 1;
            });
        });
        let (gen_before, clip_before, idx_before, data_before) = vcx.update(|_, cx| {
            let v = viewer.read(cx);
            (v.anim_gen, v.anim_clip.is_some(), v.anim_index, v.state.data.is_some())
        });
        assert_eq!(gen_before, 7, "前提：anim_gen 基线");
        assert_eq!(idx_before, 1, "前提：动画帧下标基线");
        assert!(clip_before && data_before, "前提：已挂动画片段与上屏帧");
        // release：应自停时钟 + 清动画引用 + 清上屏帧
        vcx.update(|window, cx| {
            viewer.update(cx, |v, c| v.release(window, c));
        });
        let (gen_after, clip_after, idx_after, data_after) = vcx.update(|_, cx| {
            let v = viewer.read(cx);
            (v.anim_gen, v.anim_clip.is_some(), v.anim_index, v.state.data.is_some())
        });
        assert_ne!(gen_after, gen_before, "release 应递增 anim_gen（旧动画时钟自停）");
        assert!(!clip_after, "release 应清空 anim_clip（帧 Arc 引用释放）");
        assert_eq!(idx_after, 0, "release 应重置 anim_index");
        assert!(!data_after, "release 应清空上屏帧");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-29：显示「升级替换/翻页」时，**已不在缓存**的旧显示渲染必须被 `drop_image` 下线
    /// （headless 无法观测 GPU 释放本身，以测试计数 [`Self::test_render_drops`] 代理「效果
    /// 断言」K37）。场景：旧渲染已被替换/逐出（不残留缓存）→ 安装新渲染替换显示 → 旧渲染
    /// 应 retire（drop 计数 +1）。
    #[gpui::test]
    async fn install_replace_retires_display_render_no_longer_in_cache(
        cx: &mut TestAppContext,
    ) {
        let dir = tmp_dir("retire");
        let p = dir.join("a.png");
        write_png(&p, 8, 8);
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(p.clone(), cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer);
        let mk = |c: [u8; 3]| -> Arc<RenderImage> {
            Arc::new(RenderImage::new(vec![kimg::Frame::new(kimg::RgbaImage::from_pixel(
                2,
                2,
                kimg::Rgba([c[0], c[1], c[2], 255]),
            ))]))
        };
        let old = mk([10, 20, 30]);
        let new = mk([40, 50, 60]);
        let entry = CacheEntry {
            path: p.clone(),
            render: new.clone(),
            intrinsic: (2.0, 2.0),
            bytes: 16,
            denom: 1,
            gif: None,
        };
        cx.update(|app| {
            viewer.update(app, |v, c| {
                // 旧显示渲染上屏中，但缓存已不含它（升级 `put` 顶掉 / LRU 逐出跳过当前）
                v.state.data = Some(old.clone());
                v.cache.clear();
                v.install_current(entry, &p, c);
                assert!(
                    Arc::ptr_eq(v.state.data.as_ref().unwrap(), &new),
                    "新渲染应上屏"
                );
            })
        });
        cx.run_until_parked();
        let drops = cx.update(|app| viewer.read(app).test_render_drops);
        assert!(drops >= 1, "已不在缓存的旧显示渲染应被 drop_image 下线（实为 {drops}）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-29 回看保活：旧显示渲染**仍在缓存**（翻页回看场景）→ 替换显示时绝不能提前
    /// `drop_image`（缓存 Arc 保留，回看不重解）。
    #[gpui::test]
    async fn install_switch_keeps_cached_display_render_alive(cx: &mut TestAppContext) {
        let dir = tmp_dir("keep");
        for n in ["a.png", "b.png"] {
            write_png(&dir.join(n), 4, 4);
        }
        let pa = dir.join("a.png");
        let pb = dir.join("b.png");
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(pa.clone(), cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer);
        let mk = |c: [u8; 3]| -> Arc<RenderImage> {
            Arc::new(RenderImage::new(vec![kimg::Frame::new(kimg::RgbaImage::from_pixel(
                2,
                2,
                kimg::Rgba([c[0], c[1], c[2], 255]),
            ))]))
        };
        let ra = mk([1, 2, 3]);
        let rb = mk([4, 5, 6]);
        let entry_a = CacheEntry {
            path: pa.clone(),
            render: ra.clone(),
            intrinsic: (2.0, 2.0),
            bytes: 16,
            denom: 1,
            gif: None,
        };
        let entry_b = CacheEntry {
            path: pb.clone(),
            render: rb.clone(),
            intrinsic: (2.0, 2.0),
            bytes: 16,
            denom: 1,
            gif: None,
        };
        cx.update(|app| {
            viewer.update(app, |v, c| {
                v.cache_put(pa.clone(), entry_a, c); // a 入缓存
                v.state.data = Some(ra.clone()); // a 显示
                v.install_current(entry_b, &pb, c); // 切到 b（a 仍在缓存 → 不得 retire）
            })
        });
        cx.run_until_parked();
        let (drops, cached_a) = cx.update(|app| {
            let v = viewer.read(app);
            (v.test_render_drops, v.cache_contains(&pa))
        });
        assert_eq!(drops, 0, "仍在缓存的旧显示渲染不得提前 drop_image");
        assert!(cached_a, "a 应仍在缓存供回看复用");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4C 资源占用：LRU 逐出时释放被淘汰的非当前图，但**绝不释放当前正在显示的图**。
    /// headless 无真实图集（drop_image 对零窗口为空操作、不可观测释放本身）——这里直接
    /// 驱动 `cache_put` 锁两条可观测不变量：① 非当前条目被逐出（走 drop 分支）不 panic、
    /// LRU 账一致；② 当前显示条目被逐出时 guard 跳过 drop，显示 Arc 原样保留。重负载
    /// 连环逐出由 `fast_scroll_unready_switch_defers_repaint_keeps_display` 经真实流水线触发。
    #[gpui::test]
    async fn cache_eviction_keeps_current_display_and_releases_old_entries(
        cx: &mut TestAppContext,
    ) {
        let dir = tmp_dir("r4c_evict");
        for n in ["a.png", "b.png", "c.png", "d.png"] {
            write_png(&dir.join(n), 8, 8);
        }
        let entry_for = |name: &str| {
            let p = dir.join(name);
            let bytes = std::fs::read(&p).unwrap();
            let (frames, w, h, den) = decode_background(&bytes, (10.0, 10.0)).unwrap();
            ImageViewer::build_entry(p, frames, w, h, den).unwrap()
        };
        let viewer: gpui::Entity<ImageViewer> =
            cx.update(|app| app.new(|cx| ImageViewer::new(dir.join("a.png"), cx)));
        wait_until_ready(cx, &viewer);
        wait_idle(cx, &viewer);
        let shown = cx
            .update(|app| viewer.read(app).state.data.clone())
            .expect("a 已显示");

        // 场景 1：非当前条目（b）被 LRU 逐出 → drop_image(render) 分支；账一致、不 panic、
        // 显示 Arc 不动。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.cache = LruMap::new(Some(2), usize::MAX, |e: &CacheEntry| e.bytes);
                v.cache_put(dir.join("b.png"), entry_for("b.png"), cx);
                v.cache_put(dir.join("c.png"), entry_for("c.png"), cx);
                v.cache_put(dir.join("a.png"), entry_for("a.png"), cx); // 顶掉 b（非当前）
                assert_eq!(v.cache.len(), 2, "LRU=2 只存最新两条");
                assert!(!v.cache_contains(&dir.join("b.png")), "b 应被逐出");
                assert!(v.cache_contains(&dir.join("c.png")));
                assert!(
                    Arc::ptr_eq(&shown, v.state.data.as_ref().unwrap()),
                    "逐出非当前条目不得触碰当前显示 Arc"
                );
            })
        });

        // 场景 2：当前显示条目（a==self.path）被 LRU 逐出 → guard 跳过 drop，显示 Arc 原样。
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.cache = LruMap::new(Some(1), usize::MAX, |e: &CacheEntry| e.bytes);
                v.cache_put(dir.join("a.png"), entry_for("a.png"), cx);
                v.cache_put(dir.join("b.png"), entry_for("b.png"), cx); // 顶掉 a（当前显示）
                assert_eq!(v.cache.len(), 1);
                assert!(!v.cache_contains(&dir.join("a.png")), "LRU 逐出语义不变");
                assert!(
                    Arc::ptr_eq(&shown, v.state.data.as_ref().unwrap()),
                    "当前正在显示的图被逐出时不得 drop_image（显示 Arc 必须原样保留）"
                );
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
