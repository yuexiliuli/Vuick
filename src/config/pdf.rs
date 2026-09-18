//! PDF 渲染相关配置常量（页面几何、渲染/缓存上限）。

/// PDF 页面渲染宽度（px）。
pub const PAGE_WIDTH: f32 = 840.0;
/// 相邻页间距（px）。
pub const PAGE_GAP: f32 = 14.0;
/// 页面纵向外边距（px）。
pub const PAGE_PAD_Y: f32 = 16.0;
/// 页面横向外边距（px）。
pub const PAGE_PAD_X: f32 = 20.0;
/// 渲染缩放上限（相对屏幕 DPI 的倍率）。
pub const MAX_RENDER_SCALE: f32 = 1.5;
/// 位图密度（px/pt）绝对上限：Ctrl+滚轮高倍缩放时仍保持清晰，同时防超大位图撑爆内存
/// （实际封顶 = `MAX_RENDER_SCALE * zoom`，但不超过本值）。
pub const MAX_BITMAP_SCALE: f32 = 4.0;
/// 页面缩放倍数范围（Ctrl+滚轮可调）：0.5×～4×。
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 4.0;
/// 每格滚轮（≈1 行）的缩放倍率增幅（12%）。
pub const ZOOM_STEP: f32 = 0.12;
/// 可见窗口外保留的页数（虚拟化/预取缓冲）。
pub const MARGIN: usize = 3;
/// 内存页位图保留上限（页数，超过按距离 LRU 淘汰）。
pub const CACHE_MAX_PAGES: usize = 12;
/// 并发排队给 pdfium 线程的渲染请求上限（节流，防快速滚动请求堆积）。
pub const MAX_INFLIGHT: usize = 3;
/// 滚动方向预测性预取：可见窗口外、滚动方向前方再预取 N 页进调度队列
/// （低优先级，排在可见页之后；仅帮助持续快速滚动时提前暖栅格化）。
pub const LOOKAHEAD_PAGES: usize = 3;
// ---- 拖动/写盘节流（长 PDF 来回拖滚动条时防多核 CPU 打满）----
/// 待落盘写队列上限：超出后从**最旧**丢弃（瞬态页不落盘）。落盘 WebP 无损编码在
/// 后台 executor **串行**执行（最多 1 在途），配合此上限避免编码任务逐页堆积。
pub const WRITE_QUEUE_MAX: usize = 16;
/// 滚动速度（px/帧，EMA）超过此值视为"快速拖动"：全尺寸渲染降并发、跳过预取、
/// 只由缩略图层占位——避免把 GPU/pdfium + 编码 CPU 烧在即将滚过的页面上。
pub const MOVE_THRESHOLD_PX: f32 = 160.0;
/// 快速拖动时同时排队给 pdfium 的全尺寸渲染请求上限（常态用 [`MAX_INFLIGHT`]）。
pub const MAX_INFLIGHT_MOTION: usize = 1;
/// 磁盘二级页缓存字节上限（≈ 150 页压缩后的 WebP；页面大小差异大时页数上限不可靠，
/// 32 页 A3 图纸 vs 32 页 A4 文本差 10 倍，故按字节累计、LRU 淘汰，最多保留 1 页兜底）。
pub const CACHE_DISK_BYTES: u64 = 256 * 1024 * 1024;
// ---- L0 缩略图层（拖动期间"永远有内容"，消除"加载中"白块）----
/// 缩略图目标显示宽（px，LOD 低清层）：pdfium 下采样 ~1ms、WebP ~几十 KB/张。
pub const THUMB_WIDTH: f32 = 260.0;
/// 同时在途的缩略图请求上限（独立于全尺寸 `MAX_INFLIGHT`，不挤占渲染带宽）。
pub const MAX_THUMB_INFLIGHT: usize = 4;
/// 内存中保留的缩略图页数上限（超限丢离视口中心最远者；每张 ~250KB）。
pub const THUMB_CACHE_PAGES: usize = 200;
/// 磁盘缩略图字节预算（≈ 上千张 260px WebP；热页缩略图常驻盘，回翻即时）。
pub const CACHE_THUMB_BYTES: u64 = 96 * 1024 * 1024;
/// 文本层 LRU 缓存页数上限（每页 ~20-100KB，24 页 <3MB；选中/复制/搜索用）。
pub const TEXT_CACHE_PAGES: usize = 24;
/// 同时并发预取的文本页数上限（低优先级，不抢栅格化带宽）。
pub const TEXT_INFLIGHT: usize = 4;
