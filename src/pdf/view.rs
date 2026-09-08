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
    div, img, px, rgb, App, Context, CursorStyle, Hsla, ImageSource, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, RenderImage, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{Icon, IconName};

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

/// 断言某个条目是否有下级（可折叠）：pre-order 下紧邻下一项层级更深即有子项。
fn toc_node_has_child(outlines: &[PdfOutlineEntry], idx: usize) -> bool {
    outlines
        .get(idx + 1)
        .is_some_and(|next| next.level > outlines[idx].level)
}

/// 目录折叠可见性：按 pre-order 条目判断每个下标是否应显示。///
/// 语义（与 [`toc_collapsed`](PdfViewer::toc_collapsed) 匹配）：
/// - 条目本身可见，除非它处于某个**已折叠祖先**之下（祖先 index ∈ `collapsed`）。
///   `collapsed` 只含「有下级的条目」（`level[i] < level[i+1]`）。折叠即隐藏全部后代。
/// - 祖先链判定：pre-order 下，条目 i 的祖先 = 在其之前、层级严格递增且未被其后更浅
///   条目截断的那些条目；采用「层级递减栈」维护。任一祖先折叠 → 本条隐藏。
///
/// 纯函数、无副作用，便于单测。
fn toc_visibility(
    levels: &[usize],
    collapsed: &std::collections::HashSet<usize>,
) -> Vec<bool> {
    let n = levels.len();
    let mut visible = vec![true; n];
    // 祖先栈：保存“截至当前位置仍处于祖先链上的条目下标”（层级严格递增）。
    let mut stack: Vec<usize> = Vec::with_capacity(n.min(64));
    for i in 0..n {
        let lvl = levels[i];
        // 弹出层级 >= 当前条的祖先（它们不再是 lvl 的后代链成员）
        while let Some(&top) = stack.last() {
            if levels[top] >= lvl {
                stack.pop();
            } else {
                break;
            }
        }
        // 祖先链上任一成员折叠 → 本条被隐藏
        let any_collapsed = stack.iter().any(|&a| collapsed.contains(&a));
        visible[i] = !any_collapsed;
        stack.push(i);
    }
    visible
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
    /// 已折叠的目录父项索引（`HashSet<条目下标>`），默认全部展开。
    /// 只有「有下级」的条目（下标 i 满足 level[i] < level[i+1]）才可能被折叠；
    /// 折叠后其所有后代（直到回到同层或更浅的祖先层）都不再显示。
    toc_collapsed: std::collections::HashSet<usize>,
    /// 目录面板当前宽度（px，可拖拽右缘调整）。
    toc_width: f32,
    /// 目录右缘拖拽状态：(按下的起始 x, 起始面板宽)。`None` = 未在拖拽。
    toc_drag: Option<(f32, f32)>,
    /// 右侧目录抽屉是否展开。
    toc_open: bool,
}

/// 目录面板宽度下限（px）——太窄容纳不下标题文字。
const TOC_WIDTH_MIN: f32 = 160.0;
/// 目录面板宽度上限（px）——避免把右侧 PDF 主体挤没。
const TOC_WIDTH_MAX: f32 = 480.0;
/// 目录面板默认宽度（px）。
const TOC_WIDTH_DEFAULT: f32 = 280.0;

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
            toc_collapsed: std::collections::HashSet::new(),
            toc_width: TOC_WIDTH_DEFAULT,
            toc_drag: None,
            toc_open: false,
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
                        this.toc_collapsed.clear(); // 新文档默认全部展开
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

    /// 目录面板配色：与应用工具栏色板一致（避免组件库 surface 在暗色下近黑，突兀）。
    /// 返回 (面板底, 分隔边框, 主文字, 次要文字)。
    fn toc_panel_colors(cx: &App) -> (Hsla, Hsla, Hsla, Hsla) {
        if gpui_component::Theme::global(cx).is_dark() {
            (
                rgb(0x1f2937).into(), // 面板底（同 bar_bg / 页面卡片）
                rgb(0x374151).into(), // 边框（同 bar_border）
                rgb(0xe5e7eb).into(), // 主文字（同 text_primary）
                rgb(0x9ca3af).into(), // 次要文字（同 text_minor）
            )
        } else {
            (
                gpui::white().into(), // 面板底
                rgb(0xe5e7eb).into(), // 边框
                rgb(0x111827).into(), // 主文字
                rgb(0x6b7280).into(), // 次要文字
            )
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

    /// 切换左侧目录面板开关。
    fn toggle_toc(&mut self, cx: &mut Context<Self>) {
        self.toc_open = !self.toc_open;
        cx.notify();
    }

    /// 关闭左侧目录面板（点击目录项跳转后 / 点击 ✕ 时）。
    fn close_toc(&mut self, cx: &mut Context<Self>) {
        self.toc_open = false;
        cx.notify();
    }

    /// 切换某个目录父项的折叠/展开。只有「有下级」的条目可折叠。
    fn toggle_toc_collapse(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.outlines.len() {
            return;
        }
        if toc_node_has_child(&self.outlines, index) {
            let collapsed = &mut self.toc_collapsed;
            if !collapsed.remove(&index) {
                collapsed.insert(index);
            }
            cx.notify();
        }
    }

    /// 开始拖拽目录右缘：记录 (起始 x, 起始宽)。
    fn start_toc_resize(&mut self, start_x: f32, cx: &mut Context<Self>) {
        self.toc_drag = Some((start_x, self.toc_width));
        cx.notify();
    }

    /// 拖拽中：按鼠标位移更新面板宽（自变量窗口坐标 x）。宽度 clamp 在 [MIN, MAX]。
    fn update_toc_resize(&mut self, x: f32, cx: &mut Context<Self>) {
        if let Some((start_x, start_w)) = self.toc_drag {
            self.toc_width = (start_w + (x - start_x)).clamp(TOC_WIDTH_MIN, TOC_WIDTH_MAX);
            cx.notify();
        }
    }

    /// 结束拖拽。
    fn end_toc_resize(&mut self, cx: &mut Context<Self>) {
        if self.toc_drag.is_some() {
            self.toc_drag = None;
            cx.notify();
        }
    }

    /// 测试辅助：注入任意书签（默认先有 2 条），用于驱动 TOC 面板展开的布局回归测试。
    #[cfg(test)]
    fn seed_outlines_for_test(&mut self) {
        self.outlines = vec![
            crate::pdf::PdfOutlineEntry {
                title: "测试章节".into(),
                level: 0,
                page_index: 0,
            },
            crate::pdf::PdfOutlineEntry {
                title: "测试小节".into(),
                level: 1,
                page_index: 1,
            },
        ];
    }

    /// 测试辅助：直接置位目录展开（不依赖真实书签解析结果）。
    #[cfg(test)]
    fn force_toc_open_for_test(&mut self) {
        self.toc_open = true;
    }

    /// 测试辅助：读取折叠集合（校验折叠交互结果）。
    #[cfg(test)]
    fn toc_collapsed_for_test(&self) -> &std::collections::HashSet<usize> {
        &self.toc_collapsed
    }

    /// 浮在 PDF 主体上的目录开关按钮（始终可见，有书签时）。
    fn render_toc_button(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.outlines.is_empty() {
            return Vec::new();
        }
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        let fg = colors.foreground;
        let weak = cx.weak_entity();
        let toggle = {
            let weak = weak.clone();
            Button::new("pdf-toc-toggle")
                .label(if self.toc_open { "☰ 收起" } else { "☰ 目录" })
                .ghost()
                .h(px(28.))
                .rounded_full()
                .text_size(px(12.5))
                .text_color(fg)
                .on_click(move |_, _w, cx| {
                    let _ = weak.update(cx, |this, cx| this.toggle_toc(cx));
                })
        };
        vec![div()
            .id("pdf-toc-open-btn")
            .debug_selector(|| "pdf-toc-open-btn".into())
            .absolute()
            .top(px(8.))
            .right(px(10.))
            .child(toggle)
            .into_any_element()]
    }

    /// 左侧目录面板：作为 flex 行布局的**同级兄弟**，展开时占 280px，
    /// 右侧 PDF 主体被挤压缩小（而非浮层遮挡）。
    ///
    /// 关闭或无书签时返回 `None`（完全不参与布局）。
    fn render_toc_panel(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.outlines.is_empty() || !self.toc_open {
            return None;
        }
        let outlines = self.outlines.clone();
        let levels: Vec<usize> = outlines.iter().map(|e| e.level).collect();
        let visible = toc_visibility(&levels, &self.toc_collapsed);
        let theme = gpui_component::Theme::global(cx);
        let colors = theme.semantic_tokens().colors;
        // 目录面板配色：与应用工具栏色板一致（暗色下是 1f2937 而非组件库 surface 的近黑）
        let (panel_bg, border, fg, muted) = Self::toc_panel_colors(cx);
        let weak = cx.weak_entity();

        let mut items: Vec<gpui::AnyElement> = Vec::with_capacity(outlines.len());
        for (idx, entry) in outlines.iter().enumerate() {
            if !visible[idx] {
                continue;
            }
            let title = entry.title.clone();
            let level = entry.level;
            let has_page = entry.page_index != usize::MAX;
            let page = entry.page_index;
            let weak = weak.clone();
            let fg = fg.clone();
            // 有下级 → 可折叠
            let has_child = toc_node_has_child(&outlines, idx);
            let is_collapsed = has_child && self.toc_collapsed.contains(&idx);

            let mut row = div()
                .id(SharedString::from(format!("pdf-toc-item-{page}-{idx}")))
                .debug_selector(move || format!("pdf-toc-item-{page}-{idx}"))
                .pl(px(6. + level as f32 * 14.))
                .pr(px(8.))
                .py(px(5.))
                .w_full()
                .relative()
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .group("toc-item")
                .rounded(px(4.))
                // 悬浮：主题化柔和底 + 左侧指示条 + 交互反馈；按下稍微加深
                .hover(|style| style.bg(colors.secondary).text_color(fg))
                .active(|style| style.bg(colors.secondary).opacity(0.85));

            // 左侧当前项指示条（hover 时显现，统一强调蓝）
            let indicator = div()
                .id(SharedString::from(format!("pdf-toc-ind-{idx}")))
                .debug_selector(move || format!("pdf-toc-ind-{idx}"))
                .absolute()
                .top(px(5.))
                .bottom(px(5.))
                .left(px(2.))
                .w(px(2.))
                .rounded_full()
                .bg(rgb(crate::scrollbar::RESIZE_ACCENT))
                .opacity(0.0)
                .group_hover("toc-item", |style| style.opacity(1.0));

            row = row.child(indicator);

            // 折叠箭头（有下级时）：点击只折叠/展开，不跳转
            if has_child {
                let weak = weak.clone();
                let chevron = if is_collapsed {
                    Icon::new(IconName::ChevronRight)
                } else {
                    Icon::new(IconName::ChevronDown)
                }
                .size(px(12.))
                .text_color(muted);
                let chevron_btn = div()
                    .id(SharedString::from(format!("pdf-toc-caret-{idx}")))
                    .debug_selector(move || format!("pdf-toc-caret-{idx}"))
                    .w(px(16.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .text_color(muted)
                    .group_hover("toc-item", |style| {
                        style.text_color(colors.foreground)
                    })
                    .hover(|style| style.bg(colors.secondary))
                    .on_click(move |_, _w, cx| {
                        cx.stop_propagation();
                        let _ = weak.update(cx, |this, cx| this.toggle_toc_collapse(idx, cx));
                    })
                    .child(chevron);
                row = row.child(chevron_btn);
            } else {
                // 无子项：占位宽度对齐箭头列
                row = row.child(div().w(px(16.)));
            }

            row = row
                .child(
                    div()
                        .w(px(5.))
                        .h(px(5.))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(if level == 0 {
                            rgb(crate::scrollbar::RESIZE_ACCENT).into()
                        } else {
                            muted
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(13.))
                        .text_color(if has_page { fg } else { muted })
                        .group_hover("toc-item", |style| style.text_color(fg.clone()))
                        .child(title),
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
                );

            let weak_row = weak;
            row = row.on_click(move |_, _w, cx| {
                if has_page {
                    // 跳转到目标页，但保持目录面板打开（不 close_toc）
                    let _ = weak_row.update(cx, |this, cx| {
                        this.scroll_to_page(page);
                        cx.notify();
                    });
                }
            });
            items.push(row.into_any_element());
        }

        let close_btn = {
            let weak = weak.clone();
            Button::new("pdf-toc-close")
                .label("✕")
                .ghost()
                .h(px(24.))
                .w(px(24.))
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(muted)
                .on_click(move |_, _w, cx| {
                    let _ = weak.update(cx, |this, cx| this.close_toc(cx));
                })
        };

        // 右缘拖拽手柄：宽 6px 命中区，视觉 2px 细线；按下开始拖拽。
        let resize_handle = {
            let weak = weak.clone();
            div()
                .id("pdf-toc-resize")
                .debug_selector(|| "pdf-toc-resize".into())
                .absolute()
                .right(px(0.))
                .top(px(0.))
                .bottom(px(0.))
                .w(px(6.))
                .flex()
                .items_stretch()
                .justify_center()
                .cursor(CursorStyle::ResizeColumn)
                .on_hover(cx.listener(|_this: &mut Self, _hovered: &bool, _, cx| {
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Left, move |ev, _window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.start_toc_resize(ev.position.x.as_f32(), cx)
                    });
                })
                // 命中区透明，视觉中心一条 2px 竖线（颜色：拖拽/悬浮与分割线统一的强调色，平时 border）
                .child(
                    div()
                        .w(px(2.))
                        .h_full()
                        .flex_none()
                        .rounded_full()
                        .bg(if self.toc_drag.is_some() {
                            rgb(crate::scrollbar::RESIZE_ACCENT).into()
                        } else {
                            border
                        }),
                )
        };

        Some(
            div()
                .id("pdf-toc-panel")
                .debug_selector(|| "pdf-toc-panel".into())
                .h_full()
                .w(px(self.toc_width))
                .flex_shrink_0()
                .relative()
                .bg(panel_bg)
                .border_r_1()
                .border_color(border)
                .child(
                    div()
                        .id("pdf-toc-header")
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.))
                        .h(px(40.))
                        .border_b_1()
                        .border_color(border)
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(fg)
                                .child("目录"),
                        )
                        .child(close_btn),
                )
                .child(
                    div()
                        .id("pdf-toc-list")
                        .absolute()
                        .top(px(46.))
                        .bottom(px(0.))
                        .left(px(0.))
                        .right(px(6.))
                        .overflow_y_scroll()
                        .child(
                            div().id("pdf-toc-list-inner").flex().flex_col().children(items),
                        ),
                )
                .child(resize_handle)
                .into_any_element(),
        )
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

            // 竖向+横向滚动容器 + 定位页列（水平居中，宽 = 页宽，高 = 全文档总高）
            // wrap 宽度 = max(视口宽, 页宽+边距)：窗口宽时内容居中；窗口过窄（如目录
            // 展开挤压）时 wrap 宽于视口 → 触发横向滚动条，可滚到全部页面不被裁剪。
            let view_w = f32::from(self.scroll.bounds().size.width);
            let wrap_w = view_w.max(page_width + 2.0 * PAGE_PAD_X);
            div()
                .id("pdf-scroll")
                .debug_selector(|| "pdf-scroll".into())
                .size_full()
                .overflow_y_scroll()
                .overflow_x_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div()
                        .id("pdf-column-wrap")
                        .w(px(wrap_w))
                        .flex()
                        .flex_row()
                        .justify_center()
                        .child(content),
                )
                .into_any_element()
        };

        // 外层：flex row——左 TOC 面板（展开时挤压右侧 PDF）+ 右 PDF 主体
        // 目录按钮作为 absolute 覆盖层挂在最外层 pdf-viewer（relative），
        // 不随 PDF 滚动、也不被面板挤压，始终悬浮在右上角可见。
        let toc_panel: Option<gpui::AnyElement> = self.render_toc_panel(cx);

        div()
            .id("pdf-viewer")
            .debug_selector(|| "pdf-viewer".into())
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(canvas)
            // 目录拖拽：鼠标移动/松开事件冒泡到容器统一处理（与 app 分割线同款）
            .on_mouse_move(cx.listener(
                |this: &mut Self, ev: &gpui::MouseMoveEvent, _window, cx| {
                    this.update_toc_resize(ev.position.x.as_f32(), cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _: &gpui::MouseUpEvent, _window, cx| {
                    this.end_toc_resize(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _: &gpui::MouseUpEvent, _window, cx| {
                    this.end_toc_resize(cx);
                }),
            )
            .children(toc_panel) // 左：目录面板（宽度参与布局，展开时挤压右侧）
            .child(
                div()
                    .id("pdf-main")
                    .debug_selector(|| "pdf-main".into())
                    // 右：PDF 页面主体。flex 行里填满剩余宽度；min_w(0) 允许其压缩到内容
                    // 最小宽以下——否则自动最小尺寸=页面列宽(~880px)，目录展开时不被挤压，
                    // 滚动条叠层越出窗口右侧（横向条的右半不可见）且 bounds/比例与窗口脱节。
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .relative()
                    .child(body)
                    .child(
                        div()
                            .id("pdf-scrollbar-overlay")
                            .debug_selector(|| "pdf-scrollbar-overlay".into())
                            .absolute()
                            .inset_0()
                            .child(
                                Scrollbar::vertical(&self.scroll)
                                    .mode(ScrollbarMode::Scrolling)
                                    .viewport_from_layout(),
                            )
                            // 横向滚动条：参照 markdown 预览——窗口过窄（如目录展开挤压）时
                            // scroll 容器内容宽于视口，横向溢出可滚；轴 Always 常驻可见。
                            .child(
                                Scrollbar::horizontal(&self.scroll)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            )
            .children(self.render_toc_button(cx)) // 悬浮按钮：absolute 覆盖在最外层
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
    fn toc_visibility_all_visible_when_nothing_collapsed() {
        // 默认（未折叠）→ 全部可见
        let levels = vec![0, 1, 2, 1, 0, 1];
        let collapsed = std::collections::HashSet::new();
        let v = toc_visibility(&levels, &collapsed);
        assert!(v.into_iter().all(|x| x), "未折叠时应全部可见");
    }

    #[test]
    fn toc_visibility_hides_descendants_of_collapsed_parent() {
        // 0: L0  ├1: L1  │└2: L2  ├3: L1  │└4: L2  └5: L1
        let levels = vec![0, 1, 2, 1, 2, 1];
        let mut collapsed = std::collections::HashSet::new();
        // 折叠第 0 项（L0）→ 全部后代(1..=5)隐藏，仅第 0 项可见
        collapsed.insert(0);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, false, false, false, false, false],
            "折叠 L0 应隐藏其后全部后代"
        );

        // 折叠第 1 项（L1，父 0 未折叠）→ 只隐藏它的直系后代 2；3/4/5 不受影响
        let collapsed = HashSet::from([1usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, false, true, true, true],
            "折叠 L1(1) 只应隐藏其后代 2"
        );

        // 折叠第 3 项（L1 的第二个子树）→ 隐藏 4
        let collapsed = HashSet::from([3usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, true, true, false, true],
            "折叠 L1(3) 只应隐藏其后代 4"
        );

        // 同时折叠 0 与 3 → 全部后代都隐藏
        let collapsed = HashSet::from([0usize, 3usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, false, false, false, false, false],
            "折叠 L0 后 3 及其后代 4 均被 L0 遮蔽"
        );

        // 折叠一个无子项的条目（没有下级）→ 无效果
        let collapsed = HashSet::from([2usize]); // L2，下一项 L1 < L2 → 无子项
        let v = toc_visibility(&levels, &collapsed);
        assert!(
            v.into_iter().all(|x| x),
            "无子项的条目在折叠集合中也不应影响任何可见性"
        );
    }

    #[test]
    fn toc_visibility_nested_sibling_boundary() {
        // 兄弟边界：折叠第 1 项(L1) 后，第 3 项(L1，为 0 的第二子)应仍可见。
        let levels = vec![0, 1, 2, 1, 0, 1];
        let collapsed = HashSet::from([1usize]);
        let v = toc_visibility(&levels, &collapsed);
        assert_eq!(
            v,
            vec![true, true, false, true, true, true],
            "折叠第一个 L1 不应影响第 0 的第二个孩子 L1(3)"
        );
    }

    #[test]
    fn toggle_collapse_only_affects_nodes_with_children() {
        // 构建 outlines，直接测真实判定 helper：有下级才可折叠
        let outlines = vec![
            PdfOutlineEntry { title: "A".into(), level: 0, page_index: 0 },
            PdfOutlineEntry { title: "A1".into(), level: 1, page_index: 1 },
            PdfOutlineEntry { title: "A2".into(), level: 1, page_index: 2 },
            PdfOutlineEntry { title: "B".into(), level: 0, page_index: 3 },
            PdfOutlineEntry { title: "B1".into(), level: 1, page_index: 4 },
        ];
        assert!(toc_node_has_child(&outlines, 0), "A(L0) 有子项 A1");
        assert!(!toc_node_has_child(&outlines, 1), "A1(L1) 下一项同级 → 无子项");
        assert!(!toc_node_has_child(&outlines, 2), "A2(L1) 下一项更浅 → 无子项");
        assert!(toc_node_has_child(&outlines, 3), "B(L0) 有子项 B1");
        assert!(!toc_node_has_child(&outlines, 4), "末项无子项");
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

    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext, size};

    struct PdfHost {
        viewer: Entity<PdfViewer>,
    }
    impl Render for PdfHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.viewer.clone()
        }
    }

    fn setup_host(
        cx: &mut TestAppContext,
        size: gpui::Size<gpui::Pixels>,
    ) -> (Entity<PdfHost>, &mut VisualTestContext) {
        let path = Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/N107内外饰配置表NEW.pdf"),
        );
        setup_host_with(cx, size, path)
    }

    /// 通用宿主：`path` 为 `None`/不可读时 viewer 加载失败，但**保留 seed 的目录**
    /// （不会被真实书签覆盖），适合校验目录树交互。
    fn setup_host_with(
        cx: &mut TestAppContext,
        size: gpui::Size<gpui::Pixels>,
        path: Option<std::path::PathBuf>,
    ) -> (Entity<PdfHost>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        let path = path.unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/__missing__.pdf")
        });
        let (host, cx) = cx.add_window_view(|_window, cx| {
            let viewer = cx.new(|cx| {
                let mut v = PdfViewer::new(path, cx);
                // 测试不依赖真实书签解析：直接布出 2 条目录并展开面板
                v.seed_outlines_for_test();
                v.force_toc_open_for_test();
                v
            });
            PdfHost { viewer }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size);
        // 泵驱动：后台解析的 async 任务需要跑完（真实 PdfWorker 加载），
        // 多送几帧让滚动容器以最终窗口尺寸完成布局。
        for _ in 0..200 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        (host, cx)
    }

    /// 回归：目录展开 + 窗口收窄时，右侧 PDF 主体必须被挤压进窗口内，
    /// 滚动条叠层（`pdf-scrollbar-overlay`）不得越出窗口右边界；同时 `pdf-scroll`
    /// 视口宽应按剩余可用宽度计算（否则横向滚动条的滑块几何与窗口宽度脱节）。
    #[gpui::test]
    fn toc_open_narrow_window_keeps_scrollbar_inside(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (host, cx) = setup_host(cx, size(px(980.), px(720.)));
        // 等 PdfViewer 真正加载完成（真实 pdfium 解析），在已铺页状态下校验布局。
        for _ in 0..300 {
            cx.run_until_parked();
            let loaded = cx.update(|_, cx| host.read(cx).viewer.read(cx).is_loaded());
            if loaded {
                break;
            }
        }

        // 目录面板展开时占 280px（280+40=320：TOC 固定 280 + 右侧滚动条？不，
        // TOC 只是 flex 兄弟占 280px），主体视口宽应 = 980 - 280 = 700。
        let check = |cx: &mut VisualTestContext, expect_w: f32| {
            let main = cx.debug_bounds("pdf-main").expect("pdf-main 应存在");
            let overlay = cx
                .debug_bounds("pdf-scrollbar-overlay")
                .expect("滚动条叠层应存在");
            let scroll = cx.debug_bounds("pdf-scroll").expect("pdf-scroll 应存在");
            assert!(
                (main.size.width.as_f32() - expect_w).abs() < 2.0,
                "pdf-main 宽应约为 {expect_w}（视口 - 目录 280），实际={main:?}"
            );
            assert!(
                (overlay.size.width.as_f32() - expect_w).abs() < 2.0,
                "滚动条叠层应铺满 pdf-main（约 {expect_w}），不得越出窗口，实际={overlay:?}"
            );
            assert!(
                (scroll.size.width.as_f32() - expect_w).abs() < 2.0,
                "pdf-scroll 视口宽应等于剩余可用宽（约 {expect_w}），实际={scroll:?}"
            );
            let origin = overlay.origin;
            assert!(
                origin.x.as_f32() >= -0.5 && origin.y.as_f32() >= -0.5,
                "滚动条叠层不应越出窗口左上角，actual origin={origin:?}"
            );
        };
        check(cx, 700.0);

        // 窗口收窄到 620：主体宽应 ≈ 620 - 280 = 340（此前 bug：flex 项被自动
        // 最小宽=页面列宽阻止收缩，主体仍约 880px 且滚动条右半越出窗口）。
        cx.simulate_resize(size(px(620.), px(720.)));
        for _ in 0..8 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        check(cx, 340.0);
    }

    /// 回归：点击折叠箭头后，父项子代应被隐藏（从面板 DOM 消失），再次点击展开恢复。
    ///
    /// （只校验折叠状态机 + 可见性，避免对真实 pdfium 的强依赖：用 seed 的 2 层目录。）
    #[gpui::test]
    fn toc_collapse_hides_children_and_expands_back(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        // 用空路径 → 加载失败，seed 的 2 层书签不被真实书签覆盖，便于精确校验树交互
        let (host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        // seed_outlines_for_test: idx0=L0「测试章节」、idx1=L1「测试小节」
        // 初始全展开：两条目都应渲染
        let item1 = cx.debug_bounds("pdf-toc-item-0-0").expect("父项应渲染");
        let child = cx.debug_bounds("pdf-toc-item-1-1").expect("子项初始应可见");
        assert!(child.size.height > px(0.), "子项默认可见");
        let parent_y0 = item1.origin.y;

        // 折叠父项(0)：子项(1)应进入隐藏集合且不再渲染；父行仍在
        cx.update(|_, cx| {
            let viewer = host.read(cx).viewer.clone();
            viewer.update(cx, |v, cx| v.toggle_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let collapsed_now = cx.update(|_, cx| {
            host.read(cx).viewer.read(cx).toc_collapsed_for_test().contains(&0)
        });
        assert!(collapsed_now, "父项 0 应进入折叠集合");
        assert!(
            cx.debug_bounds("pdf-toc-item-1-1").is_none(),
            "子项折叠后不应再渲染"
        );
        assert!(cx.debug_bounds("pdf-toc-item-0-0").is_some(), "父项仍应渲染");

        // 再点击一次 → 展开，子项恢复
        cx.update(|_, cx| {
            let viewer = host.read(cx).viewer.clone();
            viewer.update(cx, |v, cx| v.toggle_toc_collapse(0, cx));
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let collapsed_now = cx.update(|_, cx| {
            host.read(cx).viewer.read(cx).toc_collapsed_for_test().contains(&0)
        });
        assert!(!collapsed_now, "再次点击应展开（折叠集合移除）");
        let child = cx.debug_bounds("pdf-toc-item-1-1").expect("展开后子项应恢复");
        assert!(child.size.height > px(0.), "展开后子项应可见");
        assert!(
            (parent_y0.as_f32() - child.origin.y.as_f32()).abs() > 1.0,
            "子项应与父项不同行（在面板内垂直排列）"
        );
    }

    /// 冒烟：目录项 hover 不应 panic；指示条元素存在且位于对应行内（左侧指示条渲染）。
    #[gpui::test]
    fn toc_item_hover_renders_indicator(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (_host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        let row = cx.debug_bounds("pdf-toc-item-0-0").expect("父项应渲染");
        let ind = cx.debug_bounds("pdf-toc-ind-0").expect("指示条应渲染");
        assert!(
            ind.origin.x.as_f32() >= row.origin.x.as_f32() - 1.0
                && ind.origin.x.as_f32() <= row.origin.x.as_f32() + row.size.width.as_f32(),
            "指示条应在行水平范围内，ind={ind:?} row={row:?}"
        );

        // 把鼠标移到父项上：仅验证不 panic、布局稳定（纯样式细化，无运行时断言）。
        let center = gpui::point(
            row.origin.x + row.size.width / 2.,
            row.origin.y + row.size.height / 2.,
        );
        cx.simulate_mouse_move(center, None, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        assert!(cx.debug_bounds("pdf-toc-item-0-0").is_some(), "hover 后行仍渲染");
    }

    /// 回归：目录右缘可拖拽改变面板宽；宽度受限在 [MIN, MAX]；松开后宽度保持。
    #[gpui::test]
    fn toc_resize_drag_changes_width(cx: &mut TestAppContext) {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let (host, cx) = setup_host_with(cx, size(px(980.), px(720.)), None);

        let panel0 = cx.debug_bounds("pdf-toc-panel").expect("目录面板应存在");
        let w0 = panel0.size.width.as_f32();
        assert!((w0 - TOC_WIDTH_DEFAULT).abs() < 2.0, "默认宽应 280，实际 {w0}");

        // 取右缘中点（命中区 6px 内）作为拖拽起点
        let handle = cx.debug_bounds("pdf-toc-resize").expect("拖拽手柄应存在");
        let start = gpui::point(
            handle.origin.x + handle.size.width / 2.,
            panel0.origin.y + panel0.size.height / 2.,
        );
        // 按下 → 向右拖 60px → 松开
        cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            gpui::point(start.x + px(60.), start.y),
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.simulate_mouse_up(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }

        let panel1 = cx.debug_bounds("pdf-toc-panel").expect("目录面板应存在");
        let w1 = panel1.size.width.as_f32();
        assert!(
            (w1 - (w0 + 60.0)).abs() < 3.0,
            "右拖 60px 后宽度应约 +60，w0={w0} w1={w1}"
        );
        assert!(
            (w1 - cx
                .update(|_, cx| host.read(cx).viewer.read(cx).toc_width)
                .abs()) < 0.01,
            "拖拽后内部保存宽度应一致"
        );

        // 拖出上限：向右拖到接近窗口右缘 → 宽度应封顶 == TOC_WIDTH_MAX
        // （起点 ~337，拖到 ~970 共 ~+630，远超 MAX-340=140 的门槛）
        let handle = cx.debug_bounds("pdf-toc-resize").expect("手柄应存在");
        let start = gpui::point(
            handle.origin.x + handle.size.width / 2.,
            panel1.origin.y + panel1.size.height / 2.,
        );
        cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            gpui::point(start.x + px(630.), start.y),
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        cx.simulate_mouse_up(start, gpui::MouseButton::Left, gpui::Modifiers::none());
        for _ in 0..5 {
            cx.run_until_parked();
            let _ = cx.cx.refresh();
        }
        let w_max = cx
            .debug_bounds("pdf-toc-panel")
            .expect("目录面板应存在")
            .size
            .width
            .as_f32();
        assert!(
            (w_max - TOC_WIDTH_MAX).abs() < 1.0,
            "超大拖动应封顶在 MAX，实际 {w_max}"
        );
    }
}
