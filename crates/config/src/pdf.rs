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
/// 18 张 ≈ 7MB——覆盖快速拖动最近 ~2.5 屏，兼顾内存）。
pub const THUMB_CACHE_PAGES: usize = 18;
// ---- L0.5 中清 LOD（快速拖动时的清晰度）：260px 缩略图拉 3.2× 到 840px 必然糊，
// 加一档 ~520px 中清页，拖动时可辨识标题/段落结构，且比瓦片整屏便宜（~5ms/张）----
/// 中清目标显示宽（px）：快速拖动时优先派发/显示，作瓦片下方的占位底图。
pub const MID_WIDTH: f32 = 520.0;
/// 同时在途的中清请求上限（仅快速拖动期使用，独立于瓦片/缩略图在途）。
pub const MAX_MID_INFLIGHT: usize = 4;
/// 内存中保留的中清页数上限（每张 ~1.5MB RGBA，8 张 ≈ 12MB；中清可 ~5ms 重建，
/// 且整页封面到来即回收该页中清——低驻留即可，快速拖动最近 ~1 屏）。
pub const MID_CACHE_PAGES: usize = 8;
/// 文本层 LRU 缓存页数上限（每页 ~20-100KB，24 页 <3MB；选中/复制/搜索用）。
pub const TEXT_CACHE_PAGES: usize = 24;
/// 同时并发预取的文本页数上限（低优先级，不抢栅格化带宽）。
pub const TEXT_INFLIGHT: usize = 4;
// ---- 图片盒懒加载缓存（点击/右键命中图片 + 「复制图片」用）----
// 页图片盒数据极小（每页仅若干包围盒 + 自然尺寸，KB 级），缓存大一点无内存压力；
// 命中测试/选中描框需「点击即知」→ 可见页图片盒像文本层一样**按页预取**（懒加载），
// 无任何图片交互时零开销（worker 侧 PageImages 低优先级不抢栅格化）。
/// 图片盒 LRU 缓存页数上限（每页 KB 级，兜底可见页滑动窗口）。
pub const IMAGE_CACHE_PAGES: usize = 48;
/// 同时并发预取的图片盒页数上限（低优先级，同文本层）。
pub const IMAGE_INFLIGHT: usize = 4;
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
/// 瓦片内存字节预算（512²×4≈1MB/片；32MB ≈ 32 片，覆盖整页封面未覆盖场景——高倍缩放
/// 封面超预算走纯瓦片、或缩放出封面的瞬间。常态下有同密度封面驻留的页不再双份持有瓦片，
/// 故预算可按需下调）。超预算按「离视口中心最远」LRU 淘汰并显式 `drop_image`。
pub const TILE_CACHE_BYTES: u64 = 32 * 1024 * 1024;
/// worker 侧「未着色原图」缓存总预算（CPU 字节）：切档（`SetRecolor`）不再清 view 缓存、
/// 重调 pdfium 重栅，而是对 worker 缓存内该文档的原图重跑 `apply_recolor` 重传刷新——
/// 为保证切档能覆盖「已在屏上」的可见集合（封面 + 瓦片 + 中清/缩略），worker 需保留与
/// view 显示集相当的原图镜像。取 ~2× `TILE_CACHE_BYTES` 再加封面余量（单窗口可见窗通常
/// ≤ 4 页，高倍密度整页封面单张可达 ~12MB）→ 稳态下覆盖可见集（避免切档时个别已显瓦片
/// 因被回收而无法重着色）。内存取舍：单真值 = 未着色原图（本缓存的 CPU 字节为切档不重栅
/// 带来的**新增内存**）；着色显示副本现算现发、view 侧只驻留着色 GPU 位图（既有），
/// 不在 worker 重复驻留。超预算按「最早写入」回收最旧（单文档缓存、规模有界）。
pub const RAW_CACHE_BYTES: u64 = 96 * 1024 * 1024;
// ---- 首屏“整页锐化封面”（SumatraPDF 首遍同款）：档廉价时整页一渲即铺满首屏，
// 瓦片再在下层精修——消除“首屏几十片瓦片串行渲染 → 模糊十几秒”----
/// 封面整页位图字节上限：≤ 本值才走「整页一渲」封面（高倍缩放/超大页超预算 → 纯瓦片）。
pub const COVER_SIZE_BUDGET: u64 = 32 * 1024 * 1024;
/// 封面渲染密度硬上限（px/pt）：首帧封面用**满显示密度**（实测整页渲染 ~10-50ms，
/// Open 已近免费），一渲即锐利、不再等瓦片精修。本值只是防病理性超高倍率的护栏
/// （超预算由 [`COVER_SIZE_BUDGET`] 先拦；更高密度下可见区更小，瓦片本就填得很快）。
pub const COVER_SCALE_CAP: f32 = 4.0;
/// 同时在途的封面渲染上限：可见页封面 + 滚动方向邻居预取封面（空闲预热 1-2 页，
/// 滚到即立即锐利，不必等整页瓦片逐片来）。
pub const COVER_INFLIGHT: usize = 3;
