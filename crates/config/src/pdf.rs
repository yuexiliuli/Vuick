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
/// 高速带（VeryFast）进入阈值倍数：`MOVE_THRESHOLD_PX × 本常量` ≈ 480px/帧。
/// 中速带（Fast）已改派满密度封面，高速带再进一步——**只平移已缓存帧、不派任何新栅格**
/// （已缓存封面/瓦片原位即锐，新进视口页由 F4 预取走廊兜底，本波未实现）。退出用其 ×
/// [`MOVE_THRESHOLD_LOWER_RATIO`] 迟滞，防 480px/帧 附近横跳。
pub const MOVE_THRESHOLD_VERY_RATIO: f32 = 3.0;
// ---- L0 缩略图层（拖动期间"永远有内容"，消除"加载中"白块）。**纯内存**：
// 不落盘、不编码 WebP；超 [`THUMB_CACHE_PAGES`] 按距离 LRU 淘汰。----
/// 缩略图目标显示宽（px，LOD 低清层）：pdfium 下采样 ~1ms/张。
pub const THUMB_WIDTH: f32 = 260.0;
/// 同时在途的缩略图请求上限（独立于瓦片/中清额度）。
pub const MAX_THUMB_INFLIGHT: usize = 4;
/// 内存中保留的缩略图页数上限（超限丢离视口中心最远者；每张 ~0.37MB RGBA，
/// 12 张 ≈ 5MB——覆盖快速拖动最近 ~1.5 屏，兼顾内存；封面到位即回收该页缩略（F2））。
pub const THUMB_CACHE_PAGES: usize = 12;
// ---- L0.5 中清 LOD（快速拖动时的清晰度）：260px 缩略图拉 3.2× 到 840px 必然糊，
// 加一档 ~520px 中清页，拖动时可辨识标题/段落结构，且比瓦片整屏便宜（~5ms/张）----
/// 中清目标显示宽（px）：快速拖动时优先派发/显示，作瓦片下方的占位底图。
pub const MID_WIDTH: f32 = 520.0;
/// 同时在途的中清请求上限（仅快速拖动期使用，独立于瓦片/缩略图在途）。
pub const MAX_MID_INFLIGHT: usize = 4;
/// 内存中保留的中清页数上限（每张 ~1.5MB RGBA，6 张 ≈ 9MB；中清可 ~5ms 重建，
/// 且整页封面到来即回收该页中清——低驻留即可，快速拖动最近 ~1 屏）。
pub const MID_CACHE_PAGES: usize = 6;
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
/// 可见视口之外纵向（滚动方向）再预取一圈瓦片（边缘缓冲）：保留 1 行余量，滚动掉帧时
/// 下一行已在槽，滚动过程不闪底。
pub const TILE_MARGIN: u32 = 1;
/// 瓦片**横向**（跨列/平移方向）缓冲区：0（P1 ①）。本阅读器纵向滚动为主，横向平移仅在
/// 放大溢出时可用（`zoom>1` 且内容宽于视口）且属低频；横向整列预取在高倍/宽窗下每列 =
/// 整页行数 × ~1MB，是纯浪费——收紧到 0，避开宽轴整列进钉住集。仅纵向余量保留。
pub const TILE_MARGIN_COL: u32 = 0;
/// 同时排队给 pdfium 线程的瓦片请求上限（瓦片 ~512px、比整页便宜一个量级，可更高并发）。
pub const TILE_INFLIGHT: usize = 6;
/// 瓦片内存字节预算（512²×4≈1MB/片；32MB ≈ 32 片，覆盖整页封面未覆盖场景——高倍缩放
/// 封面超预算走纯瓦片、或缩放出封面的瞬间。常态下有同密度封面驻留的页不再双份持有瓦片，
/// 故预算可按需下调）。超预算按「离视口中心最远」LRU 淘汰并显式 `drop_image`。
/// **本值是动态预算的下限**（R3）：高倍钉住收敛下，可见格数 × 单格字节 ×
/// [`TILE_BUDGET_KEEP_FACTOR`] 可能超过本值（sf2 zoom4 宽窗 ≈30–98 片 ≈30–98MB）——
/// 见 [`TILE_CACHE_BYTES_MAX`]，动态预算在 `[本值, 上限]` 内随可见集伸缩。
pub const TILE_CACHE_BYTES: u64 = 32 * 1024 * 1024;
/// 动态瓦片预算**硬顶**（字节，R3）：预算 = `max(TILE_CACHE_BYTES, 可见格数 × 单格字节 ×
/// 1.25)`，夹在本值与无上限之间。取 128MiB：覆盖最恶劣可见窗口（如 sf2.0 zoom4 宽窗
/// ~98 片 ×1.25 ≈122MiB），且钉住集 ≤ 预算**天然成立**——稳态仍绑定工作集、非满额。
/// 再超本值（可见格 >~102）→ R4 均匀 0.5 降级兜底（宁均匀不格子、内存有界、无死锁）。
pub const TILE_CACHE_BYTES_MAX: u64 = 128 * 1024 * 1024;
/// 动态瓦片预算相对「可见钉住集」的**余量系数**（R3）：预算 = `max(固定, 可见格数 ×
/// 单格字节 × 本值)`。1.25 覆盖「可见矩形 + TILE_MARGIN 边格 + 边缘残片 + 派发在途瞬时
/// 叠加」的名义字节估算余量，令钉住集在有界内存内稳定收敛、不被逐出不震荡。
pub const TILE_BUDGET_KEEP_FACTOR: f32 = 1.25;
/// worker 侧「未着色原图」缓存总预算（CPU 字节，**CPU 重着色回退缓存**）：仅 CPU 重着色
/// 路径（GPU 门控关闭 / `GPUI_PDF_GPU_RECOLOR=0`）填充并消费——GPU 门控默认路径 0 消费者、
/// 不写缓存（见 `pdf::parser::finalize_render`）。切档（`SetRecolor`）不再清 view 缓存、重调
/// pdfium 重栅，而是对 worker 缓存内该文档的原图重跑 `apply_recolor` 重传刷新——为保证切档
/// 能覆盖「已在屏上」的可见集合（封面 + 瓦片 + 中清/缩略），worker 需保留与 view 显示集
/// 相当的原图镜像。48MB ≈ 高倍可见集（3× 整页封面 ~12MB + 瓦片 ~12MB + 余量），比旧 96MB
/// 更贴近可见集稳态（≈ 1.5× `TILE_CACHE_BYTES` + 封面余量），超可见集部分本就该被回收。
/// 内存取舍：单真值 = 未着色原图（本缓存的 CPU 字节为切档不重栅带来的**新增内存**）；
/// 着色显示副本现算现发、view 侧只驻留着色 GPU 位图（既有），不在 worker 重复驻留。
/// 超预算按「最早写入」回收最旧（单文档缓存、规模有界）。
/// 【预期行为：48MB 预算紧张下的部分刷新】「滚出再滚回」后首可视页封面恰是**最早写入**的
/// 最旧条目 → 预算紧张（≤预算的行程)时它可能已被 FIFO 挤出 → 切档（`SetRecolor`）刷新时该
/// 页封面**暂不重着色**，靠下一交互（滚回/缩放重渲）按新参数现算恢复——这是预算取舍下的
/// **预期行为**，不是缺陷（专项测试 `recolor_switch_partial_refresh_when_visible_cover_evicted`
/// 正向钉死：回传 ⊆ 仍驻留缓存的可见集子集、被挤出 key 不回传、切档 0 重栅、不崩）。
pub const RAW_CACHE_BYTES: u64 = 48 * 1024 * 1024;
// ---- 首屏“整页锐化封面”（SumatraPDF 首遍同款）：档廉价时整页一渲即铺满首屏，
// 瓦片再在下层精修——消除“首屏几十片瓦片串行渲染 → 模糊十几秒”----
/// 封面整页位图字节上限：≤ 本值才走「整页一渲」封面（高倍缩放/超大页超预算 → 纯瓦片）。
pub const COVER_SIZE_BUDGET: u64 = 32 * 1024 * 1024;
/// 封面渲染密度硬上限（px/pt）：首帧封面用**满显示密度**（实测整页渲染 ~10-50ms，
/// Open 已近免费），一渲即锐利、不再等瓦片精修。本值只是防病理性超高倍率的护栏
/// （超预算由 [`COVER_SIZE_BUDGET`] 先拦；更高密度下可见区更小，瓦片本就填得很快）。
pub const COVER_SCALE_CAP: f32 = 4.0;
/// 邻居页封面密度上限（px/pt，P1 ③）：封面窗内「不可见」的滑入/滑出邻居页封面降到
/// 本密度档（A4 每张 ~2MB 级，比满密度 4.0 的 ~31MB 降一个量级）——**可见页仍满密度**；
/// 邻居入视瞬间经既有 stale→prev_cover 桥 + 满密度重派无缝接管（防翻页闪白）。
pub const COVER_SCALE_CAP_NEIGHBOR: f32 = 2.0;
/// 单张封面位图**像素面积硬上限**（P1 ⑤，保底护栏）：量级参考 A4@4.0（≈1200 万 px/页），
/// 防极端病理性页（超大格式/超高宽）把多张封面驻留池顶爆。语义为「像素钳制」，与
/// [`COVER_SIZE_BUDGET`]（字节闸）双闸同向；当前字节闸（32MB ≈ 840 万 px）更紧时自然
/// 先触发，本值作为命名保底、将来字节闸放宽时仍兜住面积。
pub const COVER_AREA_BUDGET: u64 = 12_000_000;
/// 高倍 cover 回收的最低「截断比」（S5）：仅当 `render_scale > cover_scale × 本值`（cap 截断
/// 造成明显密度差、且该倍率下视口覆盖的瓦片量小、预算可承受）才回收 cap 截断的 cover 交给
/// 1:1 瓦片接管。**1.5 → 1.3（R5）**：动态预算（[`TILE_CACHE_BYTES_MAX`]）下高倍可见瓦片集
/// 必被钉住收敛，cap cover 的截断密度差阈值收紧（1.3×）让其在收敛后**提前退席**、消掉
/// 高倍整页 cover 的驻留峰值；仅「render_scale ≈1.3–1.5× cover_scale」的中档仍保 cover
/// （避免 4.0 vs ~5.2-6.0 处的回收-重派环；≤1.3× 属同密度/刀刃区，本不回收）。cap 不升。
pub const COVER_RETIRE_MIN_RATIO: f32 = 1.3;
/// 同时在途的封面渲染上限：可见页封面 + 滚动方向邻居预取封面（空闲预热 1-2 页，
/// 滚到即立即锐利，不必等整页瓦片逐片来）。
pub const COVER_INFLIGHT: usize = 3;
// ---- F4 空闲预取走廊（WPS「好像全部已加载」的来源）：idle 早退前把窗口±2 的
// 低成本半密度整页底图预取进槽，高速平移切入时新页短暂模糊但不「加载中」----
/// 走廊封面渲染密度（相对满密度封面 [`crate::pdf::layout::cover_scale`] 的比例）。
/// 0.5 密度 = 位图像素 1/4（~5MB/页、~5-12ms/页），走廊填得更深更便宜，「滚动即
/// 清晰」的观感足够（高速平移期只是匆匆一瞥，降速后由满密度封面/瓦片接管）。
pub const PREFETCH_COVER_DENSITY: f32 = 0.5;
/// 走廊封面单帧最多新派发数（跨帧预算）：idle 每帧 ≤2，空闲期消化、不洪起 worker 队列。
pub const PRE_FETCH_COVER_INFLIGHT: usize = 2;
/// 走廊封面显存/内存总预算（字节）：0.5 密度 A4 适配宽 ~2.6-5MB/页，40MB ≈ 8-15 页走廊。
/// 独立于窗口封面（`COVER_SIZE_BUDGET`/`COVER_INFLIGHT`），超预算按距视口中心 LRU 收回。
pub const PREFETCH_COVER_BUDGET_BYTES: u64 = 40 * 1024 * 1024;
/// 走廊窗口：可见页两侧各外扩的页数（idle 期间预取 [vis±2]，覆盖高速平移一两次大跳）。
pub const PREFETCH_COVER_MARGIN: usize = 2;
// ---- L1 滚动条快速跨页跳（直接送达落点，消灭拖完等救援的空白窗）----
/// 大跳判定阈值（页数，L1.1）：`ensure_window` 每帧比较当前可见中心页与上帧可见中心页，
/// 位移超过本值（MARGIN=3 + 走廊 ±2 已够不到的 6-8 页量级）即判定「大跳」→ 置 `jump_armed`
/// + `jump_target=当前可见中心`，VeryFast 高速带据此对**落点页**满密度派发封面（而非纯平移
/// 零派发），配合 L2 的 `nav_gen` 世代直接送达落点，消灭「拖完停手后 EMA 0.7ⁿ 衰减
/// ~150-250ms 才重新调度」的空窗。取 7：介于普通一屏快滚（3-5 页）与滚动条/目录大跳（10+
/// 页）之间；太小会把普通快滚误判成大跳而每帧 `nav_gen++` 清空在途（浪费），太大会让
/// VeryFast 纯平移拖到停稳、落点页仍长期「加载中」。
pub const JUMP_PAGES_THRESHOLD: usize = 7;
/// 停稳快进检测的连续帧数 K（L1.3）：连续 K 帧滚动位移 < [`STOP_DETECT_PX`] → 速度**直落
/// Slow**（绕过 0.7ⁿ EMA 衰减），令落点封面立刻被既有 Slow/Idle 调度消费。取 3：滚轮骤停的
/// 前 1-2 帧惯性微移（1-2px）不误触；拖拽中手指微抖（单帧小位移）也不误触——需连续 3 帧
/// 才判定停稳。
pub const STOP_DETECT_FRAMES: u32 = 3;
/// R4 均匀降级「静态帧」阈值：可见页「组不齐 + 预算触硬顶」需连续多少**静态帧**才降级到
/// 均匀 0.5 底。取 3：与 [`STOP_DETECT_FRAMES`] 同量级——瓦片在途串行渲染的瞬时缺口、
/// 滚动静置前的动量帧不误降级；连续 3 帧静态仍不齐 = 结构性超预算 → 切均匀底
/// （宁均匀不格子、内存有界、无死锁）。
pub const DEGRADE_STATIC_FRAMES: u32 = 3;
/// 停稳快进检测位移阈值（px/帧，L1.3）：连续 [`STOP_DETECT_FRAMES`] 帧 |Δy| < 本值 → 判定停稳。
/// 取 2px：小于单个滚轮 notch（~1 行）的典型位移即为「停」；大于 2px 的持续滚动不误触。
pub const STOP_DETECT_PX: f32 = 2.0;
/// 缩放手势防抖帧数（S3）：`zoom_armed` 期间连续 [`ZOOM_DEBOUNCE_FRAMES`] 帧无新 Ctrl+滚轮
/// 缩放事件 → 清 `zoom_armed` 并强制 Slow，让常态调度 `schedule_tiles` 按**最终 cscale**
/// 一次性满密收口（在途旧档由既有 scale 守卫自动弃，无需 cancelRendering）。
/// 取 3：与 [`STOP_DETECT_FRAMES`] 同量级——滚轮事件流（~16ms/档）档间间隙不误收口，
/// 同时「松手后 ~50ms 内重新锐利」（3×16ms）贴合目标可观测行为；太小则缩放中某档间
/// 间隙也会误收口抢派满密（复利雾），太大会让松手后的收口延迟在体感之上（回到「加载中」）。
pub const ZOOM_DEBOUNCE_FRAMES: u32 = 3;
/// 缩放收口瓦片**一次性出清**预算（S5/E）：防抖到期收口帧对 `cur>4` 页（cap 截断、纯瓦片
/// 供给）把单一 [`TILE_INFLIGHT`]=6 临时抬到本值，可见瓦片一次入队 → 高倍收口不再
/// 3-6 帧逐片雾窗。只抬**在途**不增驻留（`TILE_CACHE_BYTES` 不变，worker 串行渲染仍为
/// 墙钟界）。取 16：> 常规 TILE_INFLIGHT 4 倍、覆盖 2K 视口典型可见片数（6~20）大部；
/// 再多单帧洪入 worker FIFO 对「中心页先锐」无边际收益、反而挤掉后续常态节流。普通滚动
/// 不触发（仅 `zoom_armed` 清空的当帧置位）。
pub const SETTLE_TILE_BURST_MAX: usize = 16;
/// 封面密度**等效上限**（D cap 刀刃稳健化）：`render_scale` 落在 `[COVER_SCALE_CAP,
/// 本值]` 区间时，即便被 [`COVER_SCALE_CAP`] 截断（cover 4.0 vs scale 4.06 ≈98.8% 密度、
/// 1.5% 内视觉无差）也视为「同密度 cover-served」——消除 ×1.12 f32 不幂等在 4.0±1 档
/// 两侧落在 cover(锐) vs 瓦片雾窗(糊) 两翼的**时锐时糊**翻转。取 4.06：比 cap 高 1.5%，
/// 恰好包住「一档 1.12 链」在 cap 邻域的落点离散；更高则高倍页过早用 cap 封面顶 1:1 瓦片
/// （清晰度损失越过可感知线）。仅影响「是否派瓦片/是否视为同密度」的判定，不升 cap 本身。
pub const COVER_DENSITY_EPS_ZOOM: f32 = 4.06;
/// 走廊窗口随速度扩大的上限（页，L3.2）：M(v)=clamp(round(v/平均页高)+1, [`PREFETCH_COVER_MARGIN`],
/// 本值)，高速平移（VeryFast，单帧穿越 4+ 页）期走廊不可能只用 [vis±2] 盖住 → 按速度把滑窗
/// 拉大、滑窗 [vis±M(v)]。预算仍由 [`PREFETCH_COVER_BUDGET_BYTES`]=40MB LRU 封顶、
/// [`PRE_FETCH_COVER_INFLIGHT`]=2/帧 跨帧消化，扩张只影响「先派哪个/先逐哪个」，不改变总预算。
/// 取 8：0.5 密度 A4 ~2.6-5MB/页、40MB 可驻留 8-15 页，[vis±8]=17 窗口按距中心近远排序
/// 「近的先派」，预算先覆盖最可能滚到的一圈；idle 速度≈0 时 M 回落基线 2，既有多测语义不变。
pub const PREFETCH_COVER_MARGIN_MAX: usize = 8;
/// 方向感知逐出（L3.3）的「回退侧驻留加成」（页数）：`evict_prefetch_covers` 的排序键
/// `dist − bonus`，本加成只给「当前滚动方向回退侧、且在走廊窗内」的近期过页面——保这些
/// 退位页的廊封面驻留（滚回瞬间有底图，不落「加载中」）。取 2 = [`PREFETCH_COVER_MARGIN`]：
/// 只影响**同距离内的竞争顺序**，不改变任何预算/驻留计数（回退侧同距离页比远侧多驻留 2 名
/// 的量级），既有内存护栏恒成立。
pub const PREFETCH_RETURN_SIDE_BONUS: usize = 2;
