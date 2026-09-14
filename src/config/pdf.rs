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
/// 可见窗口外保留的页数（虚拟化/预取缓冲）。
pub const MARGIN: usize = 3;
/// 内存页位图保留上限（页数，超过按距离 LRU 淘汰）。
pub const CACHE_MAX_PAGES: usize = 8;
/// 并发排队给 pdfium 线程的渲染请求上限（节流，防快速滚动请求堆积）。
pub const MAX_INFLIGHT: usize = 2;
/// 磁盘二级页缓存页数（略大于内存预算，保证常见短程回翻命中）。
pub const CACHE_DISK_PAGES: usize = 32;
