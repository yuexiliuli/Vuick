//! PDF 阅读器视图：自包含、页级虚拟化（参照 gpui-pdf / zorite `gpui-pdf` 的
//! `PdfView`，裁剪掉缩放/搜索/目录等与本项目无关的能力）。
//!
//! 关键机制：
//! - **打开即显**：文件读取 + 结构解析 + 页尺寸测量都在后台线程做（便宜），
//!   完成后马上铺好全部页槽（滚动条正确）；**不**预先栅格化任何页。
//! - **按需栅格化**：每帧根据滚动位置算“可见页区间”（[`keep_window`]，含 MARGIN
//!   缓冲），只对这些页在后台栅格化；渲染按“离可见区中心最近优先”填充。
//! - **回收**：滚离视口的页释放位图 + GPU 纹理（`drop_image`），内存受限。
//! - **不闪烁**：页槽先是有尺寸占位（页号/加载中），位图到位后替换，布局稳定。
//!
//! ## 性能优化（相对旧版）
//!
//! 1. **页列 DOM 虚拟化**：`render()` 每帧只构建「可见页 + MARGIN 缓冲」这几张页面卡片
//!    并以绝对定位铺在等高的“全文档高度”容器里（[`compute_layout`]），
//!    不再把几百上千页全部 `div()` 出来布局——长文档滚动/换帧开销从 O(页数) 降到 O(可见页)。
//! 2. **磁盘二级缓存 = 原始 BGRA**：[`PageCache`] 落盘的是**去 stride 的紧凑 BGRA**，
//!    不再经 PNG 压缩。翻回已淘汰页时直接读盘 + `Frame::from_raw`（毫秒级），
//!    彻底去掉旧版的 PNG 解码（≈128ms 主线程冻结）。PNG 编码也退出 pdfium 工作线程
//!    （旧版每次渲染都在 worker 里编码，白白拖慢后续页面排队）。
//! 3. **在途请求节流** [`MAX_INFLIGHT`]：同时排队给 pdfium 线程的请求不超过 N 个。
//!    单线程 pdfium 串行渲染吞吐不变，但快速滚动时不会无限堆积过期请求——
//!    停止滚动后「正在看的那页」最多等 N-1 个在途页，不再被几十个旧请求插队。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use gpui::{
    div, img, px, rgb, App, Context, Hsla, ImageSource, InteractiveElement, IntoElement,
    ParentElement, Render, RenderImage, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

use crate::pdf::{
    render_image_from_bgra, DocId, PdfOutlineEntry, PdfWorker, RenderRequest, RenderResult,
};

/// 页面统一显示基准宽（px，视口内按比例缩放）。
const PAGE_WIDTH: f32 = 840.0;
/// 页与页之间的垂直间距（px）。
const PAGE_GAP: f32 = 14.0;
/// 页列顶部/底部内边距（px）。
const PAGE_PAD_Y: f32 = 16.0;
/// 页面左右内边距（px）。
const PAGE_PAD_X: f32 = 20.0;
/// 栅格化倍率上限：超过则不再放大（控制每个页位图像素数与内存）。
/// 840px 宽 × 1.5 ≈ 1260 device px，A4 竖版高 ≈1780px ≈ 4B×2.2M ≈ 9MB/页封顶。
const MAX_RENDER_SCALE: f32 = 1.5;
/// 可见页上下各多留存的已渲染页数（滚动时已就绪，不抖动）。
const MARGIN: usize = 3;
/// **内存**最多同时保留的已渲染位图页数（内存上限）。超出后把**离当前视口最远**的页回收
/// 到磁盘缓存；翻回时从磁盘恢复（毫秒级），不重新栅格化。
/// 只保留近视口的几页在 RAM，其余靠磁盘：几百页文档内存也稳定在几十 MB。
const CACHE_MAX_PAGES: usize = 8;
/// 同时排队给 pdfium 工作线程的**在途渲染请求上限**。pdfium 是单线程串行渲染，
/// 该上限不改变吞吐，但能遏制「快速滚动 → 请求无限堆积」：停止滚动后正在看的那页
/// 最多排在 N-1 个在途页之后，即最多等 (N-1)×单页耗时，而不是几十个旧请求。
const MAX_INFLIGHT: usize = 2;

/// 磁盘缓存文件头魔数（4 字节）+ 宽高各 4 字节 + 紧凑 BGRA 像素。
const CACHE_MAGIC: &[u8; 4] = b"GPBG";
const CACHE_HEADER: usize = 12;
/// 磁盘缓存在磁盘上最多保留的页数（LRU 淘汰到该上限）。
///
/// 光栅化本身已很快（~11ms/页，无 PNG 编码），磁盘缓存只是「翻回这几页」时免重算的
/// 安全网（对慢页/密集页尤其值）；用预算封顶，避免原始 BGRA（~2.8MB/页）把临时目录
/// 撑爆。此值略大于内存预算 [`CACHE_MAX_PAGES`]，保证常见的短程回翻都命中。
const CACHE_DISK_PAGES: usize = 32;

/// 磁盘二级页缓存：内存页位图预算淘汰后，把页以**紧凑 BGRA**（去 stride）落盘为每页一个
/// 文件；再次翻回时**直接读盘、`Frame::from_raw` 重建** `RenderImage`（毫秒级），
/// 无需重新栅格化。磁盘页数用 LRU 封顶到 [`CACHE_DISK_PAGES`]，避免原始 BGRA 撑爆临时目录。
///
/// 写入由调用方放到**后台 executor**（异步）；读回在主线程（NVMe ~1ms，可接受）。
/// 状态（关闭标记 + LRU 顺序）放在 [`CacheInner`] 里由异步写与主线程读共享。
#[derive(Debug)]
struct CacheInner {
    dir: PathBuf,
    /// 已关闭标记：`clear()` 后置位，阻止迟到的后台落盘任务重建目录。
    closed: AtomicBool,
    /// 目前在磁盘上的页号（最近使用在队尾）。
    lru: std::sync::Mutex<Vec<usize>>,
    /// 磁盘页数上限。LRU 淘汰到该上限。
    budget: usize,
}

#[derive(Clone, Debug)]
pub struct PageCache {
    inner: Arc<CacheInner>,
}

impl PageCache {
    /// 以 `pdf_path` 的文件名在系统临时目录开一个分片缓存目录。
    pub fn new(pdf_path: &Path) -> Self {
        let dir = std::env::temp_dir()
            .join("gpui-editor-pdf-cache")
            .join(fingerprint(pdf_path));
        // 沿用旧格式目录（可能残留 `.bin`/`.png` 旧缓存文件）不主动清理；读盘按魔数跳过损坏文件。
        Self {
            inner: Arc::new(CacheInner {
                dir,
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget: CACHE_DISK_PAGES,
            }),
        }
    }

    /// 从磁盘读取某页 BGRA（缺失/损坏返回 None）。供主线程快速恢复：
    /// 命中后直接 `render_image_from_bgra`，毫秒级、无冻结。
    pub fn load_bgra(&self, idx: usize) -> Option<(Vec<u8>, u32, u32)> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return None;
        }
        let path = self.inner.dir.join(format!("{idx:05}.bin"));
        let data = std::fs::read(path).ok()?;
        if data.len() < CACHE_HEADER || &data[..4] != CACHE_MAGIC {
            return None;
        }
        let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let expect = CACHE_HEADER + (w as usize) * (h as usize) * 4;
        if data.len() != expect {
            return None;
        }
        let bgra = data[CACHE_HEADER..].to_vec();
        // 标记为最近使用
        if let Ok(mut lru) = self.inner.lru.lock() {
            lru.retain(|&p| p != idx);
            lru.push(idx);
            let excess = lru.len().saturating_sub(self.inner.budget);
            if excess > 0 {
                lru.drain(0..excess);
            }
        }
        Some((bgra, w, h))
    }

    /// 清理本 PDF 的整个缓存目录（关闭标签页时调用）。
    pub fn clear(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&self.inner.dir);
    }
}

/// 后台落盘：把一页紧凑 BGRA 写成 `idx.bin`（带魔数与宽高头），并按 LRU 封顶磁盘页数。
/// 在后台 executor 执行，不触碰主线程 / pdfium 工作线程。
fn save_bgra(inner: Arc<CacheInner>, idx: usize, bgra: Vec<u8>, w: u32, h: u32) {
    if inner.closed.load(Ordering::Relaxed) {
        return;
    }
    let _ = std::fs::create_dir_all(&inner.dir);
    let mut data = Vec::with_capacity(CACHE_HEADER + bgra.len());
    data.extend_from_slice(CACHE_MAGIC);
    data.extend_from_slice(&w.to_le_bytes());
    data.extend_from_slice(&h.to_le_bytes());
    data.extend_from_slice(&bgra);
    let _ = std::fs::write(inner.dir.join(format!("{idx:05}.bin")), data);
    // 超出预算：淘汰最久未用的页（异步删除，失败静默）
    let evict: Vec<usize> = {
        let mut lru = inner.lru.lock().unwrap_or_else(|e| e.into_inner());
        lru.retain(|&p| p != idx);
        lru.push(idx);
        if lru.len() > inner.budget {
            let extra = lru.len() - inner.budget;
            lru.drain(0..extra).collect()
        } else {
            Vec::new()
        }
    };
    for page in evict {
        let _ = std::fs::remove_file(inner.dir.join(format!("{page:05}.bin")));
    }
}

/// 页槽：某页的渲染状态。
#[derive(Default, Clone)]
struct Slot {
    /// 已栅格化位图（`None` = 从未渲染或已回收）。
    image: Option<Arc<RenderImage>>,
    /// 是否正在后台栅格化（避免重复派发）。
    loading: bool,
    /// 该位图对应的渲染代次（缩放/质量变化时使旧位图失效）。
    generation: u64,
}

/// 某页在页列里的显示高度（保持宽高比）。
fn display_height((w, h): (f32, f32), page_width: f32) -> f32 {
    if w > 0.0 {
        page_width * (h / w)
    } else {
        page_width
    }
}

/// 预先计算整列布局：每页的 top 偏移（`PAGE_PAD_Y` + 前缀和）与整列总高。
///
/// 与 [`keep_window`] / [`render`](PdfViewer::render) 的绝对定位**使用同一套数学**，
/// 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。
fn compute_layout(dims: &[(f32, f32)], page_width: f32) -> (Vec<f32>, f32) {
    let mut tops = Vec::with_capacity(dims.len());
    let mut y = PAGE_PAD_Y;
    for dim in dims {
        tops.push(y);
        y += display_height(*dim, page_width) + PAGE_GAP;
    }
    let last = dims.last().copied();
    let total = match last {
        Some(dim) => tops[dims.len() - 1] + display_height(dim, page_width) + PAGE_PAD_Y,
        None => PAGE_PAD_Y * 2.0,
    };
    (tops, total)
}

/// 可见页区间（含缓冲）：与页列布局镜像，纯函数便于测试。
///
/// `scroll_y` 为内容已向下滚动的像素数（≥0）；`viewport_h` 为可见区高度。
fn keep_window(
    dims: &[(f32, f32)],
    page_width: f32,
    scroll_y: f32,
    viewport_h: f32,
) -> (usize, usize) {
    if dims.is_empty() {
        return (0, 0);
    }
    let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
    let top = scroll_y.max(0.0);
    let bottom = top + vh;
    let (tops, _) = compute_layout(dims, page_width);

    // 第一页「页面底 > 视口顶」
    let mut first = 0usize;
    for (i, dim) in dims.iter().enumerate() {
        if tops[i] + display_height(*dim, page_width) > top {
            first = i;
            break;
        }
    }
    // 最后一页「页面顶 < 视口底」
    let mut last = 0usize;
    for i in (0..dims.len()).rev() {
        if tops[i] < bottom {
            last = i;
            break;
        }
    }
    let start = first.saturating_sub(MARGIN);
    let end = (last + MARGIN).min(dims.len() - 1);
    (start, end)
}

/// 渲染倍率：以在屏宽度、显示 DPI 推导，封顶避免超大位图。
fn render_scale(page_width: f32, scale_factor: f32, page_pt_width: f32) -> f32 {
    if page_pt_width > 0.0 {
        (page_width * scale_factor / page_pt_width).clamp(0.5, MAX_RENDER_SCALE)
    } else {
        1.5
    }
}

/// 每帧的页工作规划（纯函数，便于测试）：根据每页当前是否“已渲染/正在加载/代次”，
/// 决定 (需要渲染的页, 需要淘汰位图的页)。
///
/// - **渲染**：可见区内既**未渲染**又**未在加载中**（或位图代次过期）的页。
///   正在加载的页绝不再派发任务，避免每个可见帧都重复栅格化同一页。
/// - **淘汰（延后回收 / 距离 LRU）**：仅当保留位图数超过 [`CACHE_MAX_PAGES`] 时，
///   才把**离当前视口中心最远**的页依次回收直到回到预算内。预算内任一页——无论多靠前
///   多靠后——都保留，因此「往下翻很多页再倒回去」命中缓存，不重新加载。
fn plan_page_work(
    rendered: &[bool],
    loading: &[bool],
    start: usize,
    end: usize,
    generation: u64,
    rendered_generation: &[u64],
) -> (Vec<usize>, Vec<usize>) {
    let dims_len = rendered.len();
    let mut to_render: Vec<usize> = Vec::new();
    for i in start..=end.min(dims_len.saturating_sub(1)) {
        let needs_render =
            !rendered[i] || rendered_generation.get(i).copied() != Some(generation);
        if needs_render && !loading[i] {
            to_render.push(i);
        }
    }

    let retained = rendered.iter().filter(|r| **r).count();
    if retained <= CACHE_MAX_PAGES {
        return (to_render, Vec::new());
    }
    // 预算已满：离中心最远优先淘汰（等距取索引小者，保证确定性）
    let center = (start + end) / 2;
    let mut candidates: Vec<usize> = rendered
        .iter()
        .enumerate()
        .filter(|(_, r)| **r)
        .map(|(i, _)| i)
        .collect();
    candidates.sort_by_key(|&i| (std::cmp::Reverse((i as i64 - center as i64).abs()), i));
    let mut to_evict: Vec<usize> = Vec::new();
    for i in candidates {
        if to_evict.len() >= retained - CACHE_MAX_PAGES {
            break;
        }
        to_evict.push(i);
    }
    to_evict.sort_unstable();
    (to_render, to_evict)
}
 
/// 用文件名做缓存目录指纹（防路径含非法字符）。
fn fingerprint(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pdf".into());
    // 简单散列：文件大小 + 名称字节异或，避免依赖额外 crate
    let mut hash = 0x9e37u64.wrapping_mul(name.len() as u64);
    for b in name.bytes() {
        hash = hash.wrapping_mul(131).wrapping_add(b as u64);
    }
    format!("{:016x}-{}", hash, sanitize(&name))
}

/// 只保留安全字符，避免路径分隔符/通配等被误用。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

/// PDF 阅读器：自包含的页级虚拟化视图。
pub struct PdfViewer {
    /// 文件路径（含文件名，标题用）。
    path: PathBuf,
    /// 进程级 pdfium 工作线程句柄（加载完成后为 `Some`）。
    worker: Option<PdfWorker>,
    /// 本阅读器在该工作线程中打开的文档编号。
    doc_id: DocId,
    /// 每页 (宽,高)（PDF 点）。
    dims: Vec<(f32, f32)>,
    /// 每页 top 偏移（`compute_layout` 结果，渲染定位 + 可见区推算共用）。
    page_tops: Vec<f32>,
    /// 整列总高（含上下内边距）。
    total_height: f32,
    /// 每页渲染槽。
    slots: Vec<Slot>,
    /// 渲染代次（缩放/质量变化时递增使旧位图失效）。
    generation: u64,
    /// 在途渲染请求的结果接收通道（worker 线程回传）。
    pending: HashMap<usize, mpsc::Receiver<Option<RenderResult>>>,
    /// 竖向滚动句柄（页列外层 scroll 容器）。
    scroll: ScrollHandle,
    /// 是否仍在后台解析。
    loading: bool,
    /// 加载失败信息。
    load_error: Option<String>,
    /// 阅读画布背景（页面“纸面”之外底色；None 用主题推导）。
    bg: Option<Hsla>,
    /// 待释放的旧位图（重渲染替换下来的，需在下一帧 `drop_image`）。
    pending_drops: Vec<Arc<RenderImage>>,
    /// 磁盘二级页缓存：内存淘汰后读盘即得，来回翻页不重新栅格化。
    cache: PageCache,
    /// 书签目录（标题 + 层级 + 目标页码）；无书签时为 `[]`。
    outlines: Vec<PdfOutlineEntry>,
}

impl PdfViewer {
    /// 创建阅读器并异步加载 `path`（读文件 + 解析 + 量页在后台，UI 不阻塞）。
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let cache = PageCache::new(&path);
        let mut this = Self {
            path,
            worker: None,
            doc_id: 0,
            dims: Vec::new(),
            page_tops: Vec::new(),
            total_height: 0.0,
            slots: Vec::new(),
            generation: 0,
            pending: HashMap::new(),
            scroll: ScrollHandle::new(),
            loading: true,
            load_error: None,
            bg: None,
            pending_drops: Vec::new(),
            cache,
            outlines: Vec::new(),
        };
        this.start_load(cx);
        this
    }

    /// 后台加载：读文件 + 经单例 pdfium 工作线程解析 + 量页 + 书签目录，完成后在主线程铺页槽。
    fn start_load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    // 进程级单例工作线程：打开文档 + 量页 + 书签目录，返回句柄与 DocId
                    let (worker, doc_id, dims, outlines) =
                        PdfWorker::open_and_dims_full(bytes)
                            .map_err(|e| format!("无法启动 pdfium：{e}"))?;
                    Ok::<_, String>((worker, doc_id, dims, outlines))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                match result {
                    Ok((worker, doc_id, dims, outlines)) => {
                        this.worker = Some(worker);
                        this.doc_id = doc_id;
                        this.dims = dims;
                        this.outlines = outlines;
                        let (tops, total) = compute_layout(&this.dims, this.page_width());
                        this.page_tops = tops;
                        this.total_height = total;
                        this.slots = vec![Slot::default(); this.dims.len()];
                        this.loading = false;
                        cx.notify();
                    }
                    Err(e) => {
                        this.loading = false;
                        this.load_error = Some(e);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// 滚动到指定页（目录跳转）。页码越界时忽略。
    pub fn scroll_to_page(&mut self, page_index: usize) {
        if page_index >= self.dims.len() {
            return;
        }
        let top = self.page_tops.get(page_index).copied().unwrap_or(0.0);
        self.scroll.set_offset(gpui::point(px(0.), px(-top)));
    }

    /// 页数（测试 / 状态栏使用；文档尚未加载完时为 0）。
    #[allow(dead_code)]
    pub fn page_count(&self) -> usize {
        self.dims.len()
    }

    /// 是否已加载完成（测试用）。
    #[allow(dead_code)]
    pub fn is_loaded(&self) -> bool {
        !self.loading && self.worker.is_some() && !self.dims.is_empty()
    }

    /// 设置阅读画布背景色。
    pub fn set_background(&mut self, bg: Hsla) {
        self.bg = Some(bg);
    }

    /// 画布默认底色：浅色模式浅灰、深色模式深灰。
    fn default_canvas(cx: &App) -> Hsla {
        if gpui_component::Theme::global(cx).is_dark() {
            rgb(0x111827).into()
        } else {
            rgb(0xe5e7eb).into()
        }
    }

    /// 页面“卡片”配色：纸面白 / 深色纸面，配边框。
    fn page_card_colors(cx: &App) -> (Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (rgb(0x1f2937).into(), rgb(0x374151).into())
        } else {
            (gpui::white().into(), rgb(0xd1d5db).into())
        }
    }

    /// 释放所有已上传的 GPU 纹理（关闭标签页 / 丢弃视图时调用），关闭 worker 侧文档并清空磁盘缓存。
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut drops = std::mem::take(&mut self.pending_drops);
        drops.extend(self.slots.iter_mut().filter_map(|s| s.image.take()));
        for arc in drops {
            cx.drop_image(arc, Some(window));
        }
        if let Some(worker) = &self.worker {
            if self.doc_id != 0 {
                worker.close(self.doc_id);
                self.doc_id = 0;
            }
        }
        self.cache.clear();
    }

    /// 当前页列宽度。
    fn page_width(&self) -> f32 {
        PAGE_WIDTH
    }

    /// 每帧：重算可见区间，回收远端页，派发可见页的后台栅格化。
    fn ensure_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for arc in std::mem::take(&mut self.pending_drops) {
            cx.drop_image(arc, Some(window));
        }
        if self.dims.is_empty() || self.worker.is_none() {
            return;
        }
        let generation = self.generation;

        // 1) 回收本帧已完成的在途渲染结果（GPT 主线程，BGRA 直包 + 异步落盘）
        self.collect_results(cx);

        let page_width = self.page_width();
        let scale_factor = window.scale_factor();
        let scroll_y = f32::from(-self.scroll.offset().y);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);

        // 规划：要渲染的页 + 要淘汰位图的页（保留策略见 plan_page_work）
        let rendered: Vec<bool> = self.slots.iter().map(|s| s.image.is_some()).collect();
        let loading: Vec<bool> = self.slots.iter().map(|s| s.loading).collect();
        let rendered_generation: Vec<u64> = self.slots.iter().map(|s| s.generation).collect();
        let (to_render, to_evict_idx) =
            plan_page_work(&rendered, &loading, start, end, generation, &rendered_generation);
        let mut to_evict: Vec<Arc<RenderImage>> = Vec::new();
        for i in to_evict_idx {
            if let Some(slot) = self.slots.get_mut(i) {
                if let Some(arc) = slot.image.take() {
                    slot.generation = 0;
                    to_evict.push(arc);
                }
            }
        }
        for arc in to_evict {
            cx.drop_image(arc, Some(window));
        }
        // 先尝试磁盘二级缓存：命中则直接读盘重建位图（毫秒级），不派发栅格化任务；
        // 未命中才需要真正栅格化。
        let mut need_render: Vec<usize> = Vec::new();
        for &i in &to_render {
            let cached = self
                .cache
                .load_bgra(i)
                .and_then(|(bgra, w, h)| render_image_from_bgra(bgra, w, h).ok());
            match cached {
                Some(img) => {
                    self.slots[i].loading = false;
                    self.slots[i].image = Some(img);
                    self.slots[i].generation = generation;
                }
                None => need_render.push(i),
            }
        }
        if need_render.is_empty() {
            return;
        }

        // 靠近可见区中心的页先渲染（先填正在看的那页）
        let center = (start + end) / 2;
        need_render.sort_by_key(|&i| (i as i64 - center as i64).unsigned_abs());

        // 2) 派发渲染请求给进程级 pdfium 工作线程；已在途（loading）的页绝不重复派发；
        //    同时用 MAX_INFLIGHT 节流，防止快速滚动时请求无限堆积。
        let inflight = self.pending.len();
        let budget = MAX_INFLIGHT.saturating_sub(inflight);
        if budget == 0 {
            return;
        }
        let worker = self.worker.as_ref().unwrap();
        for i in need_render.into_iter().take(budget) {
            if self.slots[i].loading {
                continue;
            }
            self.slots[i].loading = true;
            let scale = render_scale(page_width, scale_factor, self.dims[i].0);
            let rx = worker.request_render(self.doc_id, RenderRequest { page: i, scale });
            self.pending.insert(i, rx);
        }
    }

    /// 回收已完成的在途渲染结果：worker 回传 BGRA → 重建 `RenderImage` 装入内存槽
    /// （代次未变才装；被淘汰/失效的槽只清 loading 允许重投）→ **异步落盘**二级缓存。
    /// 非阻塞：仅检查已就绪通道，未完成的留到下一帧。
    fn collect_results(&mut self, cx: &mut Context<Self>) {
        let generation = self.generation;
        let finished: Vec<(usize, Option<RenderResult>)> = self
            .pending
            .iter_mut()
            .filter_map(|(i, rx)| match rx.try_recv() {
                Ok(res) => Some((*i, res)),
                Err(_) => None,
            })
            .collect();
        let mut ready: Vec<(usize, Arc<RenderImage>)> = Vec::new();
        for (i, res) in finished {
            self.pending.remove(&i);
            match res {
                Some(res) => {
                    let (w, h) = (res.width, res.height);
                    // 后台异步落盘紧凑 BGRA（写盘/LRU 淘汰不在主线程、不在 pdfium worker）
                    let cache_inner = self.cache.inner.clone();
                    let bgra = res.bgra.clone();
                    cx.background_executor()
                        .spawn(async move { save_bgra(cache_inner, i, bgra, w, h) })
                        .detach();
                    // 主线程快路径：BGRA 直接包裹（零像素循环），worker 已排好字节序
                    if let Ok(img) = render_image_from_bgra(res.bgra, w, h) {
                        ready.push((i, img));
                    }
                }
                None => {
                    if let Some(slot) = self.slots.get_mut(i) {
                        slot.loading = false; // 渲染失败，允许之后重试
                    }
                }
            }
        }
        let mut any = false;
        for (i, img) in ready {
            if let Some(s) = self.slots.get_mut(i) {
                s.loading = false;
                if generation == self.generation {
                    s.generation = generation;
                    if let Some(old) = s.image.replace(img) {
                        self.pending_drops.push(old);
                    }
                }
            }
            any = true;
        }
        if any {
            cx.notify();
        }
    }

    /// 右上角悬浮目录按钮：只有文档含书签时才渲染。
    ///
    /// 用 gpui-component 的 `Button` + `DropdownMenu`（与 markdown 预览目录一致）：
    /// 点击弹出书签列表，条目按层级缩进；点击跳转到对应页。
    fn render_toc_layer(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.outlines.is_empty() {
            return Vec::new();
        }
        let outlines = self.outlines.clone();
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        let fg = colors.foreground;
        let weak = cx.weak_entity();

        let trigger = Button::new("pdf-toc-toggle")
            .label("≡ 目录")
            .ghost()
            .h(px(28.))
            .rounded(px(6.))
            .text_size(px(12.5))
            .text_color(fg)
            .dropdown_menu(
                move |menu: PopupMenu, _window: &mut Window, cx: &mut Context<PopupMenu>| {
                    let mut m = menu;
                    let theme = gpui_component::Theme::global(cx);
                    let colors = theme.semantic_tokens().colors;
                    let fg = colors.foreground;
                    let muted = colors.muted_foreground;
                    let accent = colors.accent;
                    for entry in outlines.iter() {
                        let title = entry.title.clone();
                        let level = entry.level;
                        let has_page = entry.page_index != usize::MAX;
                        let page = entry.page_index;
                        let weak = weak.clone();
                        let fg = fg.clone();
                        let muted = muted.clone();
                        let accent = accent.clone();
                        let item = PopupMenuItem::element(move |_, _| {
                            let indent = (level as f32) * 12.0;
                            div()
                                .w_full()
                                .pl(px(indent))
                                .pr_2()
                                .py_0()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(5.))
                                        .h(px(5.))
                                        .rounded_full()
                                        .bg(if level == 0 { accent } else { muted }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_size(px(13.))
                                        .text_color(if has_page { fg } else { muted })
                                        .child(title.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(10.5))
                                        .text_color(muted)
                                        .child(if has_page {
                                            (page + 1).to_string()
                                        } else {
                                            "—".into()
                                        }),
                                )
                        });
                        let item = item.on_click(move |_, _w, cx| {
                            if has_page {
                                let _ = weak.update(cx, |this, cx| {
                                    this.scroll_to_page(page);
                                    cx.notify();
                                });
                            }
                        });
                        m = m.item(item);
                    }
                    m
                },
            );

        vec![div()
            .id("pdf-toc-wrap")
            .debug_selector(|| "pdf-toc-wrap".into())
            .absolute()
            .top(px(8.))
            .right(px(20.))
            .child(trigger)
            .into_any_element()]
    }
}

impl Render for PdfViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_window(window, cx);
        let canvas = self.bg.unwrap_or_else(|| Self::default_canvas(cx));
        let (page_bg, page_border) = Self::page_card_colors(cx);
        let page_width = self.page_width();

        // 错误 / 加载中 / 已加载三种状态
        let body: gpui::AnyElement = if let Some(err) = &self.load_error {
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(page_border)
                .child(format!("无法打开 PDF：{err}"))
                .into_any_element()
        } else if self.loading || self.dims.is_empty() {
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(14.))
                .text_color(page_border)
                .child("正在解析 PDF…")
                .into_any_element()
        } else {
            // **页列 DOM 虚拟化**：scroll 容器里放一个「全文档总高」的定位容器，
            // 只把可见页 ± MARGIN 的卡片以绝对定位铺进去。整列高度 = total_height，
            // 所以滚动条范围从一开始就正确；远页（几千页时）不参与本帧 DOM/布局。
            let scroll_y = f32::from(-self.scroll.offset().y);
            let viewport_h = f32::from(self.scroll.bounds().size.height);
            let (start, end) = keep_window(&self.dims, page_width, scroll_y, viewport_h);
            let tops = &self.page_tops;
            let total = self.total_height;
            let slots = &self.slots;

            let mut cards: Vec<gpui::AnyElement> = Vec::with_capacity(end - start + 1);
            for i in start..=end {
                let disp_h = display_height(self.dims[i], page_width);
                let top = tops[i];
                let slot_img = slots.get(i).and_then(|s| s.image.clone());
                let mut card = div()
                    .id(SharedString::from(format!("pdf-page-{i}")))
                    .absolute()
                    .top(px(top))
                    .left(px(0.))
                    .flex_shrink_0()
                    .w(px(page_width))
                    .h(px(disp_h))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(page_border)
                    .overflow_hidden();
                card = match slot_img {
                    Some(page_img) => card.child(
                        img(ImageSource::Render(page_img))
                            .w(px(page_width))
                            .h(px(disp_h)),
                    ),
                    None => card
                        .bg(page_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(page_border)
                                .child(format!("第 {} 页 加载中…", i + 1)),
                        ),
                };
                cards.push(card.into_any_element());
            }

            let content = div()
                .id("pdf-pages")
                .debug_selector(|| "pdf-pages".into())
                .relative()
                .w(px(page_width))
                .h(px(total))
                .children(cards);

            // 竖向滚动容器 + 定位页列（水平居中，宽 = 页宽，高 = 全文档总高）
            div()
                .id("pdf-scroll")
                .debug_selector(|| "pdf-scroll".into())
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div()
                        .id("pdf-column-wrap")
                        .w_full()
                        .flex()
                        .flex_row()
                        .justify_center()
                        .px(px(PAGE_PAD_X))
                        .child(content),
                )
                .into_any_element()
        };

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .bg(canvas)
            .child(body)
            .child(
                div()
                    .id("pdf-vscroll-overlay")
                    .debug_selector(|| "pdf-vscroll-overlay".into())
                    .absolute()
                    .inset_0()
                    .child(
                        Scrollbar::vertical(&self.scroll)
                            .mode(ScrollbarMode::Scrolling)
                            .viewport_from_layout(),
                    ),
            )
            .children(self.render_toc_layer(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a4() -> (f32, f32) {
        (595.0, 842.0) // A4 竖版（pt）
    }

    #[test]
    fn keep_window_covers_only_visible_plus_margin() {
        // 30 页 A4，页宽 840 → 每页高约 1189；视口 600px 高，滚到第 10 页附近
        let dims: Vec<(f32, f32)> = vec![a4(); 30];
        let scroll_y = 10.0 * page_step(840.0);
        let (s, e) = keep_window(&dims, 840.0, scroll_y, 600.0);
        // 第 10 页应可见，区间含 MARGIN 缓冲且不越界
        assert!(s <= 10 && 10 <= e, "第 10 页应在可见区间 [{s},{e}]");
        assert!(e - s + 1 >= 2 * MARGIN + 1, "区间宽度应至少为可见页 + 2*MARGIN");
        assert!(e <= 29, "末页不越界");
    }

    #[test]
    fn keep_window_empty_dims_returns_zero() {
        assert_eq!(keep_window(&[], 840.0, 0.0, 600.0), (0, 0));
    }

    #[test]
    fn compute_layout_is_monotonic_and_bounded() {
        let dims: Vec<(f32, f32)> = vec![a4(); 100];
        let (tops, total) = compute_layout(&dims, 840.0);
        assert_eq!(tops.len(), 100);
        assert_eq!(tops[0], PAGE_PAD_Y, "首页 top = 顶部内边距");
        for i in 1..100 {
            assert!(
                tops[i] > tops[i - 1],
                "top 应严格递增：第 {i} 页 top={} < 前一页 top={}",
                tops[i],
                tops[i - 1]
            );
        }
        let last = tops[99] + display_height(dims[99], 840.0);
        assert!(
            (total - (last + PAGE_PAD_Y)).abs() < 0.01,
            "总高 = 末页底 + 底部内边距：total={total}, last={last}"
        );
    }

    #[test]
    fn plan_renders_only_unrendered_visible_pages() {
        // 10 页，第 0 页已渲染且代次一致；视口 [0,2] → 只应渲染 1、2
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let loading = [false; 10];
        let gens = [0u64; 10];
        let (render, evict) = plan_page_work(&rendered, &loading, 0, 2, 0, &gens);
        assert_eq!(render, vec![1, 2], "仅未渲染的可见页应列入渲染");
        assert!(evict.is_empty(), "无越界已渲染页不应淘汰");
    }

    #[test]
    fn plan_skips_pages_already_loading() {
        // 可见页 1 正在加载 → 不应重复派发渲染任务（CPU 占用暴增的元凶）
        let rendered = [true, false, false, false, false, false, false, false, false, false];
        let mut loading = [false; 10];
        loading[1] = true;
        let gens = [0u64; 10];
        let (render, _) = plan_page_work(&rendered, &loading, 0, 2, 0, &gens);
        assert!(
            !render.contains(&1),
            "正在加载的页 1 不应重复派发（杜绝每帧重栅格化）"
        );
        assert!(render.contains(&2), "其余未渲染可见页仍应渲染");
    }

    #[test]
    fn plan_keeps_rendered_pages_within_budget() {
        // 200 页：仅第 10 页渲染，其余未渲染。预算 CACHE_MAX_PAGES，远未满 → 第 10 页必须保留。
        let mut rendered = vec![false; 200];
        let mut gens = vec![0u64; 200];
        let loading = vec![false; 200];
        rendered[10] = true;
        gens[10] = 0;
        let (_r, evict) = plan_page_work(&rendered, &loading, 50, 55, 0, &gens);
        assert!(evict.is_empty(), "预算未满不应淘汰任何页（即使远在 40 页之外）");
    }

    #[test]
    fn plan_evicts_farthest_first_when_over_budget() {
        // 200 页都渲染（超预算 CACHE_MAX_PAGES），视口在 [100,105]。
        // 应淘汰离中心最远的页（最前/最后），而可见页绝不能被淘汰。
        let n = 200usize;
        let rendered = vec![true; n];
        let gens = vec![0u64; n];
        let loading = vec![false; n];
        let (r, evict) = plan_page_work(&rendered, &loading, 100, 105, 0, &gens);
        assert!(r.is_empty(), "全部已渲染，无需再渲染");
        // 回收到预算内
        assert_eq!(n - evict.len(), CACHE_MAX_PAGES, "保留数应等于预算");
        assert!(evict.contains(&0) && evict.contains(&199), "最远两端应先被淘汰");
        for i in 100..=105 {
            assert!(!evict.contains(&i), "可见页 {i} 绝不能被淘汰");
        }
    }

    #[test]
    fn scrolling_down_and_back_does_not_reload_rendered_pages() {
        // 模拟用户「往下翻很多页再倒回去」：全程用 plan_page_work 推进状态机，
        // 已渲染页只要未超预算、未被 LRU 淘汰，就绝不会进入 to_render（即不再“加载”）。
        let n = 400usize;
        let mut rendered = vec![false; n];
        let mut gens = vec![0u64; n];
        let loading = vec![false; n];

        // 逐窗口滚动：从第 0 页往下翻 100 次（每次窗口推进 2 页），再倒回翻 100 次
        for step in -100i64..=100 {
            let base = (50 + step * 2).clamp(0, (n as i64) - 6) as usize;
            let (start, end) = (base, (base + 5).min(n - 1));
            let (to_render, to_evict) = plan_page_work(&rendered, &loading, start, end, 0, &gens);
            // 记录本轮“需要重新加载”的页（应只含“从未渲染过”的页）
            for i in &to_render {
                assert!(
                    !rendered[*i],
                    "第 {i} 页已渲染却仍被要求渲染（往回翻又加载了！）step={step}"
                );
                rendered[*i] = true;
                gens[*i] = 0;
            }
            for i in &to_evict {
                rendered[*i] = false;
            }
        }
    }

    #[test]
    fn keep_window_before_first_paint_assumes_viewport() {
        // viewport_h ≤ 1 时回退默认高度，首帧也应覆盖从首页开始的若干页
        let dims: Vec<(f32, f32)> = vec![a4(); 5];
        let (s, e) = keep_window(&dims, 840.0, 0.0, 0.0);
        assert!(s == 0, "首帧应从首页开始，实际 s={s}");
        assert!(e <= 4, "不越界，实际 e={e}");
    }

    #[test]
    fn display_height_preserves_aspect() {
        let h = display_height(a4(), 840.0);
        assert!((h - 840.0 * 842.0 / 595.0).abs() < 0.01);
    }

    #[test]
    fn render_scale_capped_and_sane() {
        // 页宽 595pt，在屏 840px @ 1x DPI → ~1.41x，夹在 [0.5, MAX_RENDER_SCALE]
        let s = render_scale(840.0, 1.0, 595.0);
        assert!(s > 0.5 && s <= MAX_RENDER_SCALE);
        // 超大 DPI（Retina 2x）也要封顶
        let s2 = render_scale(840.0, 2.0, 100.0);
        assert!(s2 <= MAX_RENDER_SCALE);
    }

    #[test]
    fn page_cache_bgra_roundtrip() {
        // 临时目录里写一页 BGRA、再读回，验证文件头 + 像素完整
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 用测试目录构造（隔离，避免与真实缓存混用）
        let cache = PageCache {
            inner: Arc::new(CacheInner {
                dir: dir.clone(),
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget: CACHE_DISK_PAGES,
            }),
        };
        for idx in [0usize, 7usize] {
            let w = 16 + idx as u32;
            let h = 32u32;
            let mut bgra = Vec::new();
            for v in 0..(w * h) as u32 {
                bgra.extend_from_slice(&[(v & 0xff) as u8, ((v >> 8) & 0xff) as u8, 0x7f, 0xff]);
            }
            save_bgra(cache.inner.clone(), idx, bgra, w, h);
        }
        let (back, pw, ph) = cache.load_bgra(0).expect("第 0 页应读回");
        assert_eq!((pw, ph), (16, 32), "宽高应一致");
        assert_eq!(back.len(), 16 * 32 * 4, "像素字节数应一致");
        let (back7, pw7, ph7) = cache.load_bgra(7).expect("第 7 页应读回");
        assert_eq!((pw7, ph7), (23, 32));
        assert_eq!(back7[0..4], [0, 0, 0x7f, 0xff], "首像素 BGRA 应一致");
        // 不存在的页 → None
        assert!(cache.load_bgra(99).is_none(), "缺失页应返回 None");
        // 关闭后拒读（模拟已释放）
        cache.clear();
        assert!(cache.load_bgra(0).is_none(), "clear 后不应再读");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_ignores_corrupt_files() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-corrupt",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00000.bin"), b"not-a-pdf-cache").unwrap();
        std::fs::write(dir.join("00001.bin"), b"GPBG" ).unwrap();
        let cache = PageCache {
            inner: Arc::new(CacheInner {
                dir: dir.clone(),
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget: CACHE_DISK_PAGES,
            }),
        };
        assert!(cache.load_bgra(0).is_none(), "魔数不对应忽略");
        assert!(cache.load_bgra(1).is_none(), "头不全应忽略");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_respects_disk_budget() {
        // 预算 = 小数字：写入超过预算的页后，最久未用的页文件应被删掉（LRU 淘汰）
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-lru",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = PageCache {
            inner: Arc::new(CacheInner {
                dir: dir.clone(),
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget: 4,
            }),
        };
        for idx in 0..8usize {
            let bgra = vec![0x11u8, 0x22, 0x33, 0xff].repeat(64); // 8x8 像素
            save_bgra(cache.inner.clone(), idx, bgra, 8, 8);
        }
        // 写入 0..8 → 预算 4，最旧的 0..3 应被淘汰，4..7 仍在
        for idx in 0..4usize {
            assert!(
                cache.load_bgra(idx).is_none(),
                "预算内最旧的页 {idx} 应被 LRU 淘汰"
            );
        }
        for idx in 4..8usize {
            assert!(
                cache.load_bgra(idx).is_some(),
                "最近的页 {idx} 应保留在磁盘"
            );
        }
        // 读 4 会把它标记为最近使用 → 下一次写 8 时淘汰的是 5
        cache.load_bgra(4);
        let bgra = vec![0xffu8; 8 * 8 * 4];
        save_bgra(cache.inner.clone(), 8, bgra, 8, 8);
        assert!(cache.load_bgra(4).is_some(), "最近用过的页 4 不应被淘汰");
        assert!(cache.load_bgra(5).is_none(), "最久未用的页 5 应被淘汰");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 与 `keep_window` 同款布局的一页（高 + 间距）步长（测试辅助）。
    fn page_step(page_width: f32) -> f32 {
        display_height(a4(), page_width) + PAGE_GAP
    }
}
