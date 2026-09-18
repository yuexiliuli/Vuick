//! PDF 渲染相关配置常量（页面几何、渲染/缓存上限）。

/// PDF 页面渲染宽度（px）。
pub const PAGE_WIDTH: f32 = 840.0;
/// 相邻页间距（px）。
pub const PAGE_GAP: f32 = 14.0;
/// 页面纵向外边距（px）。
pub const PAGE_PAD_Y: f32 = 16.0;
/// 页面横向外边距（px）。
pub const PAGE_PAD_X: f32 = 20.0;
/// 渲染缩放上限（相对显示 DPI 的倍率）：实际封顶 = `MAX_RENDER_SCALE × zoom × scale_factor`。
/// 高 DPI 屏上渲染尺寸必须先够到“显示物理像素”（宽 = `page_width × DPI`），否则 GPU 对
/// 低分辨率位图上采样拉伸 → 文字/图像发糊；本常量是“自然物理尺寸”上允许的额外余量。
pub const MAX_RENDER_SCALE: f32 = 1.5;
/// 位图密度（px/pt）绝对上限：Ctrl+滚轮高倍缩放或超高 DPI 时仍保持清晰。
/// 内存不再由位图密度承载——瓦片渲染（见下）每片恒 512px，密度再高也只把大页切成更多
/// 小片、按视口按需渲染，故阈值可以放得很宽（4× → 16×）。
pub const MAX_BITMAP_SCALE: f32 = 16.0;
/// 页面缩放倍数范围（Ctrl+滚轮可调）：0.5×～4×。
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 4.0;
/// 每格滚轮（≈1 行）的缩放倍率增幅（12%）。
pub const ZOOM_STEP: f32 = 0.12;
/// 可见窗口外保留的页数（虚拟化/预取缓冲）。
pub const MARGIN: usize = 3;
/// 整页位图内存保留上限（页数）。内容层已瓦片化、不再走整页位图，本常量仅供
/// [`crate::pdf::layout::plan_page_work`] 的回归测试锚点使用。
pub const CACHE_MAX_PAGES: usize = 8;
/// 滚动速度（px/帧，EMA）超过此值视为"快速拖动"：改派缩略/中清占位、暂停瓦片派发，
/// 避免把 GPU/pdfium 烧在即将滚过的页面上。
pub const MOVE_THRESHOLD_PX: f32 = 160.0;
/// 速度迟滞退出比率：**进入**快速模式仍用 [`MOVE_THRESHOLD_PX`]，**退出**用它×本
/// 比率（更小阈值）——临界速度下保持上一状态，避免 LOD 降级/恢复反复横跳。
pub const MOVE_THRESHOLD_LOWER_RATIO: f32 = 0.7;
// ---- L0 缩略图层（拖动期间"永远有内容"，消除"加载中"白块）。**纯内存**：
// 不落盘、不编码 WebP；超 [`THUMB_CACHE_PAGES`] 按距离 LRU 淘汰。----
/// 缩略图目标显示宽（px，LOD 低清层）：pdfium 下采样 ~1ms/张。
pub const THUMB_WIDTH: f32 = 260.0;
/// 同时在途的缩略图请求上限（独立于瓦片/中清额度）。
pub const MAX_THUMB_INFLIGHT: usize = 4;
/// 内存中保留的缩略图页数上限（超限丢离视口中心最远者；每张 ~0.37MB RGBA，
/// 40 张 ≈ 15MB——够覆盖快速拖动最近 ~5 屏，兼顾内存）。
pub const THUMB_CACHE_PAGES: usize = 40;
// ---- L0.5 中清 LOD（快速拖动时的清晰度）：260px 缩略图拉 3.2× 到 840px 必然糊，
// 加一档 ~520px 中清页，拖动时可辨识标题/段落结构，且比瓦片整屏便宜（~5ms/张）----
/// 中清目标显示宽（px）：快速拖动时优先派发/显示，作瓦片下方的占位底图。
pub const MID_WIDTH: f32 = 520.0;
/// 同时在途的中清请求上限（仅快速拖动期使用，独立于瓦片/缩略图在途）。
pub const MAX_MID_INFLIGHT: usize = 4;
/// 内存中保留的中清页数上限（每张 ~1.5MB RGBA，16 张 ≈ 24MB；中清可 ~5ms 重建，
/// 低驻留即可——快速拖动最近 ~2 屏，瓦片补齐后即被覆盖/淘汰）。
pub const MID_CACHE_PAGES: usize = 16;
/// 文本层 LRU 缓存页数上限（每页 ~20-100KB，24 页 <3MB；选中/复制/搜索用）。
pub const TEXT_CACHE_PAGES: usize = 24;
/// 同时并发预取的文本页数上限（低优先级，不抢栅格化带宽）。
pub const TEXT_INFLIGHT: usize = 4;
// ---- 瓦片（Tile）渲染：SumatraPDF 式分块，内存按「视口」而非「页面积」封顶 ----
/// 瓦片边长（位图像素）：每片独立后台栅格化。内存 = 可见瓦片数 × TILE_SIZE² × 4，
/// 视口 2K 屏（1920×1080 @1x）约 3×3=9 片 ≈ 9MB，不再随整页缩放面积爆炸。
pub const TILE_SIZE: u32 = 512;
/// 瓦片出血（位图像素）：四周多渲染 2px，消除相邻瓦片拼接缝（跨格的粗线条/阴影被
/// 裁到片缘）。SumatraPDF / Chrome 的 PDF 拼片同款做法。
pub const TILE_BLEED: u32 = 2;
/// 可见视口之外再预取一圈瓦片（边缘缓冲）。
pub const TILE_MARGIN: u32 = 1;
/// 同时排队给 pdfium 线程的瓦片请求上限（瓦片 ~512px、比整页便宜一个量级，可更高并发）。
pub const TILE_INFLIGHT: usize = 6;
/// 瓦片内存字节预算（512²×4≈1MB/片；96MB ≈ 96 片，覆盖数屏滚动余量）。超预算按
/// 「离视口中心最远」LRU 淘汰并显式 `drop_image` 下线 GPU 纹理。
pub const TILE_CACHE_BYTES: u64 = 96 * 1024 * 1024;
