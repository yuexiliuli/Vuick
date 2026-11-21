//! PDF 底层原语（pdfium-render）：定位 DLL、进程级单工作线程、多文档逐页栅格化。
//!
//! pdfium 是 native 内核（Edge 同源），单页栅格化快，首次浏览即流畅。
//!
//! 本模块已按「根 + 同名目录」拆为 8 个子模块（纯模块重组/可见性升格，无逻辑改动）：
//! `pdfium`（DLL 引导）/ `types`（全部数据型）/ `writeback`（写回/导出/注解）/ `cache`
//! （raw 缓存预算）/ `render`（渲染簇）/ `grid`（网格缓存）/ `extract`（文本/图片抽取）/
//! `search`（检索簇）。根模块保留「消息与主循环核心」：`WorkerMsg` + [`PdfWorker`] +
//! [`PdfWorker::run`] 大循环 + `shared_worker` / `enqueue_msg` / `fpdf_last_error` /
//! `next_doc_id`。各子模块的对外项经下方 `pub use X::*`（glob）统一重导出，旧路径
//! `pdf_core::parser::X` 保持不变。
//!
//! ## 单例工作线程架构
//!
//! **进程级单例后台线程** [`shared_worker()`]：第一次用到时启动；工作线程在自己的
//! 作用域内持有一个 `Pdfium`（唯一一次 `new`）与一个 `HashMap<DocId, PdfDocument>`，
//! 通过 mpsc 接收 `Open / Dims / Render / Close` 命令。所有 [`PdfViewer`] 拿到的
//! [`PdfWorker`] 只是这个单例句柄的克隆（`Arc`），真正干活始终是同一线程——既绕开了
//! "只能绑定一次"，也让多文档共存（各阅读器各占一个 `DocId`）。
//!
//! [`PdfViewer`]: super::view::PdfViewer

mod cache;
mod extract;
mod grid;
mod pdfium;
mod render;
mod search;
mod types;
mod writeback;

pub use cache::*;
pub use extract::*;
pub use grid::*;
pub(crate) use pdfium::*;
pub(crate) use render::*;
pub use search::*;
pub use types::*;
pub use writeback::*;

use std::collections::{HashMap, VecDeque};
use std::os::raw::c_int;
use std::path::PathBuf;
use std::sync::{Arc, mpsc, OnceLock};

use anyhow::{anyhow, Context as _, Result};
use pdfium_render::prelude::*;

use crate::recolor::RecolorParams;

enum WorkerMsg {
    Open(
        DocId,
        Vec<u8>,
        mpsc::Sender<Result<OpenPayload>>,
    ),
    #[allow(dead_code)] // 回归测试渲染整页（文本对齐、贴片比对）使用
    Render(DocId, RenderRequest, mpsc::Sender<Option<RenderResult>>),
    /// F4 预取走廊的**半密度整页预览**渲染（0.5× 封面密度，`request_render_preview` 发送）：
    /// 与整页 `Render` 同优先（render_q）、同几何（`render_page` 按 `req.scale` 出全页位图），
    /// 但按 `RawKind::Mid` 语义**不入 raw 缓存、不参与重着色刷新**——廊封面是瞬态低清底图
    /// （就像中清/缩略一样），raw 入缓存只会挤出可见封面/瓦片原图（预算空转、切档无消费方）。
    RenderPreview(DocId, RenderRequest, mpsc::Sender<Option<RenderResult>>),
    /// 瓦片渲染（主内容路径）：只栅格化一页的一格（~512px）。与全尺寸 Render 同一
    /// 优先级（视口内容优先），worker 内实现见 [`render_tile`]。
    RenderTile(DocId, TileRequest, mpsc::Sender<Option<TileResult>>),
    /// L0.5 中清页渲染（~520px × DPI，~5ms/张）。优先级低于全尺寸 Render、高于缩略图
    /// Thumb：快速拖动时用中清页保证可辨识度，同时比全清便宜 ~6×。
    /// `scale_factor` 使目标宽度按显示 DPI 推导（DPI>1 时预览仍锐利，不经 GPU 上采样发糊）。
    Mid(DocId, usize, f32, mpsc::Sender<Option<RenderResult>>),
    /// L0 缩略图渲染（低清晰度、~1ms/张）。优先级从高到低：Render/RenderTile > Mid >
    /// Thumb > Text——拖动期间先铺中清/缩略占位，瓦片随后补齐。
    Thumb(DocId, usize, mpsc::Sender<Option<RenderResult>>),
    /// 网格视图（Grid 档）缩略图渲染：按目标卡宽（逻辑 px）× DPI 的目标像素宽渲染，
    /// 结果与 [`WorkerMsg::Thumb`] 同构（`RenderResult`，view 直接可画）。
    /// **独立网格缓存**（预算 LRU，绝不与 THUMB/MID/raw/cover 互扰）＋**背景信道**
    /// 调度（**最低优先级**：render > mid > thumb > rest > grid，只在 render/mid/thumb/
    /// rest 均空时才出队，见 run 循环 3.5）——网格模式下绝不阻塞/饿死阅读器任何既有
    /// 请求（含 Open/Close/Text/Search 等 rest 类）。worker 内实现见 [`render_grid`]。
    Grid(
        DocId,
        usize,
        f32,
        f32,
        mpsc::Sender<Option<RenderResult>>,
    ),
    /// **网格带宽预熟（本线）**：view 进入网格 / 网格卡宽变化 / 滚动重定中心时推送，携带
    /// **全清档**与**低清快速档**两档目标卡宽（逻辑 px）× DPI 的目标像素宽口径（`card_width ×
    /// scale_factor`，与 `Grid` 消息同构）+ **带宽几何**（[`GridPrewarmGeom`]：中心页 + 近/中带
    /// 页区间）。worker 侧开启「以视口中心页为核心的水平带宽」空闲预熟：四桶全空 + 无在途
    /// 检索/刷新时，把当前文档近带页经 [`render_grid`] **两档**（全清 + 低清）、中带页**仅
    /// 低清**直接算好写进 `grid_cache`（不经 view round-trip、不重开 pdfium），每渲 1 页回主
    /// 循环查队列——任何真实消息（含阅读模式请求）到达即被抢占（预熟只在全部队列空时推进，
    /// 优先级天然最低，绝不耽误阅读）。
    /// 文档关闭/切换/重着色（recolor）时预熟游标重置（见 run 循环各分支）。
    /// 预熟从**中心页起双向交替外扩**（近带全清+低清、中带仅低清、更远不预熟），窗口刷齐即
    /// 自然 idle 停（**不再整本推进**）——长文档预熟有界常驻几十 MB，整本 242p 超
    /// GRID_CACHE_FULL_DOC_MAX 256MB 触发的 LRU 滑动窗 / 永不完成负资产根治。滚动把中心重定
    /// → view 推送新几何 → worker 从新中心重扫（旧窗口已缓存页快速跳过）。两档都预熟因为
    /// **滚动风暴走低清桶、idle 升级走全清桶**——任一方向/时刻的滚动命中缓存才能真零闪
    /// （低清已在途的页全清升级由 idle 重派；预熟只是把两档都补齐进 cache，滚动只命中不重栅）。
    SetGridPrewarm(DocId, f32, f32, f32, GridPrewarmGeom),
    Close(DocId),
    /// 请求某页文本层数据（懒加载：仅选中/复制/搜索时调用）。**低优先级**：
    /// worker 调度保证绝不排在栅格化（Render）之后执行。
    Text(DocId, usize, mpsc::Sender<Option<PageText>>),
    /// 请求某页的**光栅图片盒列表**（懒加载：仅点击/右键命中图片时按需调用）。
    /// 低优先级（同 Text），回传该页全部 raster Image 对象的 PDF 点包围盒 + 自然分辨率
    /// （空 vec = 该页无图片；None = 文档/页不存在）。
    PageImages(DocId, usize, mpsc::Sender<Option<Vec<PdfImageBox>>>),
    /// 请求某页某图片对象按**自然分辨率**取回紧凑 BGRA 像素（「复制图片」专用；
    /// 应用滤镜/掩码后的 processed 位图）。`obj_index` 对不上 / 取位图失败 → None。
    ImagePixels(
        DocId,
        usize,
        usize,
        u32,
        u32,
        mpsc::Sender<Option<PdfImagePixels>>,
    ),
    /// 请求文档级检索（懒加载，低优先级；全页文本层扫描）。
    Search(DocId, String, mpsc::Sender<Option<PdfSearchResults>>),
    /// **写回注释并落盘**：worker 内把内存 bytes 载入「自有 raw 句柄」
    /// （`FPDF_LoadMemDocument64`）→ 在 raw doc 上建注释 → `FPDF_SaveAsCopy`
    /// （自制 `FPDF_FILEWRITE`，先写同目录临时文件再原子替换 `output_path`）。
    /// 成功回传实际写入的注释条数，并更新内存 bytes（后续操作基于新内容）。
    /// viewer 收到成功后走 Close→Open 同 path 流程重载（见 view 的 modify 相关方法）。
    Annotate(DocId, Vec<AnnotateItem>, PathBuf, mpsc::Sender<Result<usize>>),
    /// 插入空白页并落盘：`at_index` = 新页插入位置（0 基；= page_count 即末尾追加）。
    /// 成功回传**新页数**。
    InsertBlankPage(DocId, usize, f32, f32, PathBuf, mpsc::Sender<Result<usize>>),
    /// 删除页面并落盘（index 越界或仅剩 1 页 → Err）。成功回传**新页数**。
    DeletePage(DocId, usize, PathBuf, mpsc::Sender<Result<usize>>),
    /// 把单页提取成独立 PDF 文件（raw 句柄：`FPDF_CreateNewDocument` + `FPDF_ImportPages`）。
    ExtractPage(DocId, usize, PathBuf, mpsc::Sender<Result<()>>),
    /// 显式把当前内存 bytes 重新编码落盘到 `path`（无内容修改的另存/刷新）。
    Save(DocId, PathBuf, mpsc::Sender<Result<()>>),
    /// **内存级**插入空白页（待保存模型）：只改 worker 内 stores 内存工作副本（新镜像
    /// 用 [`save_raw_doc_to_bytes`] 产出），**不碰磁盘**。成功回传 [`MemOpResult`]
    /// （before=操作前镜像 / after=新镜像 / count=新页数）。与 [`WorkerMsg::InsertBlankPage`]
    /// 区别：后者落盘到 `output_path`，本消息仅内存。
    MemOpInsert(DocId, usize, f32, f32, mpsc::Sender<Result<MemOpResult>>),
    /// **内存级**删除页面（待保存模型）：守卫同磁盘版（越界 / 仅剩 1 页 → Err），
    /// 只改 stores、不碰磁盘。成功回传 [`MemOpResult`]。
    MemOpDelete(DocId, usize, mpsc::Sender<Result<MemOpResult>>),
    /// **内存级**重排页序（待保存模型）：`new_order` 为 0 基**全排列**（长度 = 当前页数、
    /// 0..n 每值恰一次；任何不合法 → 立即 Err 且文档不变、无副作用）。生效机制 =
    /// `FPDF_CreateNewDocument` + 按新序「值连续的升序段」逐段 `FPDF_ImportPages`（pagerange
    /// 只能表达升序连续段；任意排列按段序多次 import 追加成目标序，对齐既有页操作
    /// [`WorkerMsg::ExtractPage`] 的 fps 精度/保真口径，不做字节级保真）。重排后总页数不变。
    /// 只改 stores、不碰磁盘。成功回传 [`MemOpResult`]。
    MemOpReorder(DocId, Vec<usize>, mpsc::Sender<Result<MemOpResult>>),
    /// **内存级**批量删除页面（待保存模型）：`pages` 为 0 基待删页数组；逐页校验
    /// （每页 < 当前页数、去重后全有效；非法 → Err）+ 保留至少 1 页（对齐单页删除
    /// 「不能删除仅剩的 1 页」不变式），随后按索引**降序**循环走既有单页内存删除
    /// （升序删会因下标位移错删）。只改 stores、不碰磁盘。成功回传 [`MemOpResult`]。
    MemOpBatchDelete(DocId, Vec<usize>, mpsc::Sender<Result<MemOpResult>>),
    /// **提取多页到新文件**（wave7b）：`pages` 为 0 基待提取页数组，**可为任意顺序/非连续**、
    /// 允许少于全页（副本语义：按给定序逐段 `FPDF_ImportPages` 追加成新文档并落盘
    /// `out_path`，**不改变当前文档 / 不更新 stores**）。严格校验（K122/K124）：每页
    /// < 当前页数、去重后均有效、非空；非法 → 立即 Err 零副作用（不产生任何文件）。
    /// 成功回传写入文件的页数。
    ExportPages(DocId, Vec<usize>, PathBuf, mpsc::Sender<Result<usize>>),
    /// **拆分区间到新文件并移除**（wave7b）：`(start, end)` 含端点的 0 基区间。行为 =
    /// ① 区间页按**原序**提取为新文档落盘 `out_path` + ② 从当前 stores 镜像**移除**该区间
    /// （降序删，对齐批量删除姿势）。严格校验：区间合法（start ≤ end、end < 页数）、
    /// 剩余 ≥1 页（防 0 页文档）；非法 → Err 零副作用（不产生文件、当前文档不变）。
    /// 成功回传 `(新文件页数, MemOpResult)`——`MemOpResult.count` = 移除区间后的当前页数，
    /// `before/after` 供撤销与重载。
    SplitToFile(
        DocId,
        (usize, usize),
        PathBuf,
        mpsc::Sender<Result<(usize, MemOpResult)>>,
    ),
    /// **合并另一 PDF 到当前（追加）**（wave7b）：`src_bytes` 为**已由 view 侧读端走过大小
    /// 上界（[`PDF_MERGE_MAX_SRC_BYTES`]）**的合并源字节（view 侧元数据预检 + 有界读盘后才
    /// 发字节消息——本消息**不再读文件**，pdfium worker 绝不因合并源读盘而阻塞）。把
    /// `src_bytes` 全部页 `FPDF_ImportPages` 追加到当前文档**尾部**（index = 当前页数）。
    /// 严格校验：页数 > 0；失败 → Err 且当前文档不变。替换 stores 镜像（撤销 = 恢复操作前
    /// 镜像）。成功回传 [`MemOpResult`]（count = 新总页数）。
    MergeFromBytes(DocId, Vec<u8>, mpsc::Sender<Result<MemOpResult>>),
    /// 用给定 bytes 直接替换 stores 镜像（待保存模型撤销「页面操作」用；本消息不做任何
    /// pdfium 操作、零开销）。回传替换后镜像长度（存在性校验）。
    MemRestoreBytes(DocId, Vec<u8>, mpsc::Sender<Result<usize>>),
    /// 读回 stores 当前内存镜像（撤销快照 / 测试 MD5 / shell 另存共用；None=文档不存在）。
    MemGetBytes(DocId, mpsc::Sender<Option<Vec<u8>>>),
    /// stores 物化状态只读探针（T3 DIAG；**不触发物化**）：None=尚未物化，Some=已物化长度。
    MemState(DocId, mpsc::Sender<Option<usize>>),
    /// 读回某页注释（only-read，pdfium-render 高层 `page.annotations()`，供测试断言）。
    /// 低优先级，与 Text/Search 同位（不抢栅格化）。
    ReadAnnotations(DocId, usize, mpsc::Sender<Result<Vec<AnnotationInfo>>>),
    /// **纸墨重着色参数切换（按文档）**：只更新文档重着色参数（`None` = 直通），**不回传
    /// 已渲位图**。重着色刷新由独立的 [`WorkerMsg::RecolorRefresh`] 承担（view 在切档时
    /// 先发本消息置参数、再发刷新请求——二事经单通道 FIFO 保序，首批栅格化即用新参数）。
    /// 第三字段 `epoch` = view 侧当前**重着色世代号**（[`PdfViewer::recolor_epoch`]）：
    /// worker **采纳**该值（`max(当前, epoch)`、不独立递增）——修 BLOCK：开档时序中
    /// worker 接线前的多次 set_recolor 只经 `apply_loaded` 补推一次，若 worker 侧自行计数
    /// 会永远落后 view（旧色结果全部被世代守卫丢弃 → CPU 路径整页空白）。view 是世代
    /// 唯一真源，worker 只镜像；通道 FIFO 保证 view 发送值单调。
    /// by `run` 在收到消息**当下**按 `doc_id` 落到「文档→参数」映射（早于同批次任何渲染
    /// 出队），不排队。**必须按文档键控**：worker 是进程级单例、多阅读器（多窗口/多标签）
    /// 共享；若用单槽全局值，不同窗口不同偏好会互相覆盖、把错误纸墨渲到对方页面上。
    /// 只改显示像素，绝不写回文件 / 不改 pdfium 解析。
    ///
    /// 【切档不重栅决策留痕】旧语义：切档 → view 清全窗页槽 + 重置门控 → 下一帧按新参数
    /// 全量重派 → worker 重调 pdfium 重栅（数百 ms，本需求根治对象）。新语义：worker 缓存
    /// **未着色原图**（[`RawEntry`]），切档只对缓存原图重跑 [`apply_recolor`] 现算新着色
    /// 副本回传刷新可见区——**不失效缓存槽、不重置门控、不重调 pdfium 栅格**。
    SetRecolor(DocId, Option<RecolorParams>, u64),
    /// **导航世代跳变（按文档，L1/L2）**：view 在检测到大跳时 `nav_gen++` 并推送本消息告知
    /// worker——worker 侧只**采纳**（`max(当前, 消息)`）为文档级 `navigate_gen` 镜像，绝不自行
    /// 递增（view 是唯一真源）。worker 出队 render 类消息（Render/RenderTile/RenderPreview/Mid/
    /// Thumb）时，若该消息发送时携带的 enqueue-n_gen < `navigate_gen[doc]` → **直接 drop 不渲**
    /// （只渲染最新落点，快速连续跳的中间页不再白白烧 pdfium 带宽）；在飞项（已出队、阻塞
    /// pdfium）无法中途取消，渲完 view 侧 `nav_gen++` 已清掉对应 pending（rx drop，`let _ = tx.send`
    /// 静默失败）→ 丢弃。与 SetRecolor 同路径：sweep 应用、经单通道 FIFO 保证先行于同批渲染出队。
    /// 按文档键控（进程级单 worker 多阅读器共享，绝不能退化成单槽全局值）。
    Navigate(DocId, u64),
    /// **缩放世代步进（按文档，Z2）**：view 每次缩放（`adjust_zoom` 新档）`zoom_epoch++` 并推送
    /// 本消息告知 worker——worker 侧只**采纳**（`max(当前, 消息)`）为文档级 `zoom_epoch_mirror`
    /// 镜像（view 是唯一真源）。worker 出队 render 类消息时，该消息发送时携带的 enqueue-zoom
    /// stamp < `zoom_epoch_mirror[doc]` → **直接 drop 不渲**（停在 render_q 的中间档 cover/瓦片/
    /// preview 不再排队空耗 pdfium 时隙——Z2 队列堆叠的根治；最终档请求 stamp==当前镜像必过）。
    /// 与 [`WorkerMsg::Navigate`] 同路径（sweep 应用、单通道 FIFO 保序）。**独立于 nav_gen**：
    /// 滚动/大跳时代不变，防浊化导航世代语义；缺省=0 → 常规非缩放零影响。控制消息（Text/Search
    /// 等非 render）stamp 恒 0 → 不受本 drop 影响。
    SetZoomEpoch(DocId, u64),
    /// **重着色刷新（按文档，只刷可见集 + 分批分帧）**：`keys` = view 侧枚举的「显示槽
    /// 真实需要的可见集」（[`RecolorKey`]，按近中心排序），worker 对原始缓存逐条现算新
    /// 着色副本、经 `tx` 分批回传（见 [`RECOLOR_BATCH_BYTES`]），**不触碰 raw 单真值**。
    /// 窗外 / 已淘汰条目不在此集合 → 不参与本次回传（保留 raw，滚回重取时现算）。
    /// 同参短路（worker 端）：与上次已完成的刷新参数一致时直接断开通道（不重复回传）。
    RecolorRefresh(DocId, Vec<RecolorKey>, mpsc::Sender<RecolorEntry>),
    /// 读回某文档 raw 缓存条目数（只读；测试/基准用于「全缓存 vs 可见集」比例证据）。
    RawCacheLen(DocId, mpsc::Sender<usize>),
    /// 读回某文档**网格缩略图缓存**条目数（只读；预熟/常驻测试用——断言「近+中带带宽刷齐
    /// 与「往返后带宽常驻（不逐出）」，见 `grid_cache_len`；注意每页近带可占全清+低清两档
    /// 条目、中带仅低清一档——**整本补齐不再是语义**，长文档只保以中心为核心的带宽）。
    GridCacheLen(DocId, mpsc::Sender<usize>),
    /// 读回某文档**网格缩略图缓存**全部 `(页, 分桶宽)` 条目（只读；带宽驻留测试用——逐页
    /// 断言近带全清+低清双档 / 中带仅低清 / 远带无预熟缓存，见 `grid_cache_buckets`）。
    GridCacheBuckets(DocId, mpsc::Sender<Vec<(usize, u32)>>),
    /// 读回某文档当前 `navigate_gen` 镜像（只读；L1/L2 世代同步测试用，见
    /// [`PdfWorker::navigate_gen`]）。文档未打开 / 已关闭 → 0。
    NavigateGen(DocId, mpsc::Sender<u64>),
}

/// 进程级单例工作线程句柄。所有 [`PdfViewer`] 通过克隆它共享同一个 pdfium 线程。
#[derive(Clone)]
pub struct PdfWorker {
    /// 工作通道：item = `(enqueue-n_gen: u64, enqueue-zoom: u64, WorkerMsg)`（L2.2 通道包装 +
    /// Z2 缩放 stamp）。
    /// `n_gen` 在发送时由 view 打戳为当时 `nav_gen`；worker 出队 render 类消息时与文档级
    /// `navigate_gen` 镜像比对，`n_gen < navigate_gen[doc]` → 直接 drop 不渲。`zoom` 同理
    /// 打戳为当时 `zoom_epoch`，出队 render 类与 `zoom_epoch_mirror[doc]` 比对，旧档（缩放
    /// 中间档）请求直接 drop。控制消息（SetRecolor/Navigate/SetZoomEpoch/Close）不带世代
    /// 语义（sweep 当下应用），发送时打 0。
    tx: mpsc::Sender<(u64, u64, WorkerMsg)>,
}

/// 全局单例：第一次调用 [`open_and_dims`](PdfWorker::open_and_dims) 时启动并持有。
static SHARED: OnceLock<PdfWorker> = OnceLock::new();

/// 取进程级单例工作线程的句柄；首次调用时启动线程并完成唯一一次 pdfium 绑定。
fn shared_worker() -> Result<&'static PdfWorker> {
    if let Some(w) = SHARED.get() {
        return Ok(w);
    }
    let dll = pdfium_dll_path()?.clone();
    let (tx, rx) = mpsc::channel::<(u64, u64, WorkerMsg)>();
    std::thread::Builder::new()
        .name("pdfium-render".into())
        .spawn(move || PdfWorker::run(rx, dll))
        .context("无法启动 pdfium 渲染线程")?;
    Ok(SHARED.get_or_init(|| PdfWorker { tx }))
}

impl PdfWorker {
    /// 打开文档并取每页尺寸（结构解析，快）。成功则返回 (DocId, dims)，
    /// 便于调用方同时得到文档句柄与页面布局数据。书签目录由 [`Self::open_and_dims_full`] 取。
    pub fn open_and_dims(bytes: Vec<u8>) -> Result<(Self, DocId, PageDims)> {
        let (w, id, dims, _outlines) = Self::open_and_dims_full(bytes)?;
        Ok((w, id, dims))
    }

    /// 打开文档并取每页尺寸 + 书签目录（结构解析，快）。
    /// 成功则返回 (DocId, dims, outlines)，便于调用方同时得到文档句柄与页面布局、目录。
    pub fn open_and_dims_full(
        bytes: Vec<u8>,
    ) -> Result<(Self, DocId, PageDims, Vec<PdfOutlineEntry>)> {
        let worker = shared_worker()?;
        let (tx, rx) = mpsc::channel();
        let doc_id = next_doc_id();
        let _ = worker.tx.send((0, 0, WorkerMsg::Open(doc_id, bytes, tx)));
        let (dims, outlines) = match rx.recv() {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(anyhow::anyhow!("{e}")),
            Err(_) => return Err(anyhow::anyhow!("pdfium 工作线程已退出")),
        };
        Ok((worker.clone(), doc_id, dims, outlines))
    }

    /// 请求渲染某页（异步）。生产用于「首屏整页锐化封面」（廉价密度一渲铺满首屏，
    /// 见 `view.rs` 的封面调度），测试亦用于回归/基准比对。返回接收结果的通道。
    /// `nav_gen` = view 当时导航世代（L2.2 enqueue-n_gen 打戳；worker 出队时 n_gen < `navigate_gen[doc]`
    /// 直接 drop 不渲）。非大跳常态滚动 view 传 0（worker 侧 navigate_gen 基线 0，无 drop）。
    pub fn request_render(
        &self,
        doc_id: DocId,
        req: RenderRequest,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self
            .tx
            .send((nav_gen, zoom_epoch, WorkerMsg::Render(doc_id, req, tx)));
        rx
    }

    /// 请求渲染某页的一格瓦片（异步、主内容路径）。返回接收结果的通道。
    /// `nav_gen` 同 [`Self::request_render`]（L2.2 enqueue-n_gen 打戳）。
    pub fn request_tile(
        &self,
        doc_id: DocId,
        req: TileRequest,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<TileResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self
            .tx
            .send((nav_gen, zoom_epoch, WorkerMsg::RenderTile(doc_id, req, tx)));
        rx
    }

    /// 请求某页的 L0 缩略图（异步、低清晰度）。worker 按目标宽度推导低缩放，
    /// 结果与全尺寸同构（`RenderResult`），供阅读器做拖动占位 / 回翻即时内容。
    /// `nav_gen` 同 [`Self::request_render`]（L2.2 enqueue-n_gen 打戳）。
    pub fn request_thumb(
        &self,
        doc_id: DocId,
        page: usize,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self
            .tx
            .send((nav_gen, zoom_epoch, WorkerMsg::Thumb(doc_id, page, tx)));
        rx
    }

    /// 请求某页的 L0.5 中清页（异步、~520px × DPI）。优先级介于瓦片/全尺寸与缩略图之间：
    /// 快速拖动时用中清页保证可辨识度。`scale_factor` 参与目标宽度推导（高 DPI 屏上
    /// 预览仍锐利）。`nav_gen` 同 [`Self::request_render`]（L2.2 enqueue-n_gen 打戳）。
    pub fn request_mid(
        &self,
        doc_id: DocId,
        page: usize,
        scale_factor: f32,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((
            nav_gen,
            zoom_epoch,
            WorkerMsg::Mid(doc_id, page, scale_factor, tx),
        ));
        rx
    }

    /// 请求某页的**网格视图缩略图**（异步）。`card_width` = 目标卡宽（逻辑 px），
    /// `scale_factor` = 显示 DPI——目标像素宽 = `card_width × DPI`（同 `render_mid` 的
    /// den 换算姿势），worker 先向 [`GRID_BUCKET_PX`](config::pdf::GRID_BUCKET_PX) 量化
    /// 分桶再按桶渲染：相邻自然宽度落同桶 → 命中独立网格缓存复用、不重栅。结果与缩略图
    /// 同构（`RenderResult`，view 直接可画、按页落槽）。**独立网格缓存 + 背景信道**：
    /// 不抢/不逐出阅读器 THUMB/MID/raw/cover 驻留。`nav_gen` 同 [`Self::request_render`]
    /// （L2.2 enqueue-n_gen 打戳）。
    pub fn request_grid(
        &self,
        doc_id: DocId,
        page: usize,
        card_width: f32,
        scale_factor: f32,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((
            nav_gen,
            zoom_epoch,
            WorkerMsg::Grid(doc_id, page, card_width, scale_factor, tx),
        ));
        rx
    }

    /// **网格带宽预熟开启/更新（本线）**：view 进入网格 / 网格卡宽变化 / 滚动重定中心时
    /// 调用，携带全清档卡宽 × DPI + 带宽几何（[`GridPrewarmGeom`]）。worker 据此开启「四桶
    /// 全空时以视口中心页为核心做近/中带水平带宽预熟」（最低优先级，任何真实消息到达即被
    /// 抢占）。低清快档风暴期间 `Grid` 消息携带的是低清卡宽，不能据此设定全清预熟目标——故
    /// 单设本轻量控制消息（`rest_q` 外单独 sweep 当下应用，不入桶）。
    /// `quick_card` = 低清快速档目标卡宽（view 已按 [`PdfViewer::grid_quick_target_px`] 换算，
    /// worker 不再重算 0.5×/64 下限——近带两档（全清 + 低清）、中带仅低清都预熟，滚动风暴
    /// （低清桶）与 idle（全清桶）均命中缓存零重栅；`geom` 划定近/中带页区间，更远不预熟）。
    pub fn set_grid_prewarm(
        &self,
        doc_id: DocId,
        full_card: f32,
        quick_card: f32,
        scale_factor: f32,
        geom: GridPrewarmGeom,
    ) {
        let _ = self.tx.send((
            0,
            0,
            WorkerMsg::SetGridPrewarm(doc_id, full_card, quick_card, scale_factor, geom),
        ));
    }

    /// F4 预取走廊：请求渲染某页的**半密度整页预览**（异步；`scale` = 0.5× `cover_scale`）。
    /// 与普通封面同级优先级（render_q 视口内容优先），但结果**不入 raw 缓存、不参与重着色
    /// 刷新**（瞬态底图，同中清/缩略语义），节省 48MB 预算、不挤占切档刷新的可见原图镜像。
    /// `nav_gen` 同 [`Self::request_render`]（L2.2 enqueue-n_gen 打戳）。
    pub fn request_render_preview(
        &self,
        doc_id: DocId,
        req: RenderRequest,
        nav_gen: u64,
        zoom_epoch: u64,
    ) -> mpsc::Receiver<Option<RenderResult>> {
        let (tx, rx) = mpsc::channel();
        let _ = self
            .tx
            .send((nav_gen, zoom_epoch, WorkerMsg::RenderPreview(doc_id, req, tx)));
        rx
    }

    /// **导航世代跳变（L1/L2）**：view 检测到大跳、`nav_gen++` 后推送本消息。worker 侧按文档
    /// 采纳（`max`）为 `navigate_gen` 镜像，出队时据此 drop 旧世代 render 类请求。
    pub fn navigate(&self, doc_id: DocId, nav_gen: u64) {
        let _ = self.tx.send((0, 0, WorkerMsg::Navigate(doc_id, nav_gen)));
    }

    /// **缩放世代步进（Z2）**：view 每次缩放（新档）`zoom_epoch++` 后推送本消息。worker 侧按
    /// 文档采纳（`max`）为 `zoom_epoch_mirror` 镜像，出队 render 类请求时据此 drop 旧档
    /// （中间档 cover/瓦片/preview）——队列不再被旧档占住，最终档必达（时锐时糊根治）。
    pub fn set_zoom_epoch(&self, doc_id: DocId, zoom_epoch: u64) {
        let _ = self
            .tx
            .send((0, 0, WorkerMsg::SetZoomEpoch(doc_id, zoom_epoch)));
    }

    /// 关闭文档，释放 pdfium 侧资源（阅读器关闭时调用）。
    pub fn close(&self, doc_id: DocId) {
        let _ = self.tx.send((0, 0, WorkerMsg::Close(doc_id)));
    }

    /// 设置（或清除）`doc_id` 文档的纸墨重着色参数（`None` = 直通无重着色）。只影响该
    /// 文档显示像素，不写回文件。worker 在收到消息当下按文档应用（早于同批次渲染出队），
    /// view 随后重渲的瓦片即用新参数。按文档键控（见 [`WorkerMsg::SetRecolor`]）。
    /// **只更新参数、不回传已渲位图**：切档的「重着色刷新」由
    /// [`request_recolor_refresh`](Self::request_recolor_refresh) 单独承担（圈可见集、
    /// 分批分帧回传，不重调 pdfium 栅格）。打开补推（apply_loaded）只置参数：之后栅格化
    /// 会自带着色出口，无需回传。
    /// `epoch` = view 侧当前重着色世代号：worker 采纳（镜像）它而非自行计数——开档时序
    /// worker 接线前的多次 set_recolor 经补推折叠成一次消息，靠携带的最终世代对齐（BLOCK）。
    pub fn set_recolor(&self, doc_id: DocId, recolor: Option<RecolorParams>, epoch: u64) {
        let _ = self.tx.send((0, 0, WorkerMsg::SetRecolor(doc_id, recolor, epoch)));
    }

    /// 请求一次**重着色刷新（只刷可见集）**：`keys` = view 侧枚举的「显示槽真实需要的
    /// 可见集」（[`RecolorKey`]，按近中心排序），worker 对原始缓存逐条现算新着色副本、
    /// 经返回通道**分批分帧**回传（view 替换显示位图）。**不失效缓存槽、不重置门控、
    /// 不重调 pdfium 栅格**——切档即「对可见集原图重跑内核」，根治旧「清槽全量重栅」的
    /// 夜间档加载慢。同参短路（worker 端）：与上次已完成刷新参数一致时通道立刻断开。
    /// 按文档键控（见 [`WorkerMsg::RecolorRefresh`]）。
    pub fn request_recolor_refresh(
        &self,
        doc_id: DocId,
        keys: Vec<RecolorKey>,
    ) -> mpsc::Receiver<RecolorEntry> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::RecolorRefresh(doc_id, keys, tx)));
        rx
    }

    /// 读回 `doc_id` 文档当前 raw 缓存条目数（同步；只读，测试/基准用「全缓存 vs 可见集」
    /// 比例证据）。文档未打开 / 已关闭 → 0。
    pub fn raw_cache_len(&self, doc_id: DocId) -> usize {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::RawCacheLen(doc_id, tx)));
        rx.recv().unwrap_or(0)
    }

    /// 读回 `doc_id` 文档当前 `navigate_gen` 镜像（同步；只读，L1/L2 世代同步测试用）。
    /// 文档未打开 / 已关闭 → 0。**view 是导航世代唯一真源**：本方法只读 worker 侧采纳值，
    /// 用于断言「view 推进世代后 worker 镜像经 Navigate 消息对齐」（复用 w1 世代探针姿势）。
    pub fn navigate_gen(&self, doc_id: DocId) -> u64 {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::NavigateGen(doc_id, tx)));
        rx.recv().unwrap_or(0)
    }

    /// 读回 `doc_id` 文档当前**网格缩略图缓存**条目数（同步；只读，本批带宽预熟/常驻测试用——
    /// 断言「近+中带带宽刷齐」与「往返滚动后带宽常驻不逐出」）。注意每页近带可占全清+低清
    /// 两档条目、中带仅低清一档（条目数随分桶数变，不锁死 == 页数）；**整本补齐不再是语义**，
    /// 长文档只保以中心为核心的带宽。文档未打开 / 已关闭 → 0。
    pub fn grid_cache_len(&self, doc_id: DocId) -> usize {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::GridCacheLen(doc_id, tx)));
        rx.recv().unwrap_or(0)
    }

    /// 读回 `doc_id` 文档当前**网格缩略图缓存**全部 `(页, 分桶宽)` 条目（同步；只读，带宽
    /// 驻留测试用——逐页断言近带「全清桶 + 低清桶双档」/ 中带「仅低清桶」/ 远带「无预熟
    /// 缓存」，见 `grid_band_residency_near_full_mid_low_far_none`）。文档未打开 / 已关闭 → 空。
    pub fn grid_cache_buckets(&self, doc_id: DocId) -> Vec<(usize, u32)> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::GridCacheBuckets(doc_id, tx)));
        rx.recv().unwrap_or_default()
    }

    /// 请求某页文本层数据（异步、低优先级）。返回接收结果的通道。
    ///
    /// 只应在用户开始选中/搜索时按需调用（懒加载：不交互则零开销）。
    /// 调用方须在结果到达后自行判断是否仍需要：滚动/缩放已使页面过期时直接丢弃
    /// （P2 会绑定 generation 校验，此处只建立契约）。
    pub fn request_text(&self, doc_id: DocId, page: usize) -> mpsc::Receiver<Option<PageText>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::Text(doc_id, page, tx)));
        rx
    }

    /// 请求某页的**光栅图片盒列表**（异步、低优先级，同 Text 不抢栅格化）。
    /// 返回接收结果的通道；文档/页不存在 → `None`，页无图片 → `Some([])`。
    /// 只在点击/右键命中图片时按需调用（懒加载：无交互则零开销）。
    pub fn page_images(&self, doc_id: DocId, page: usize) -> mpsc::Receiver<Option<Vec<PdfImageBox>>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::PageImages(doc_id, page, tx)));
        rx
    }

    /// 请求某页某图片对象按 `w × h` 自然分辨率取回紧凑 BGRA 像素（异步、低优先级）。
    /// 返回接收结果的通道；对象下标失效 / 取位图失败 → `None`。
    pub fn image_pixels(
        &self,
        doc_id: DocId,
        page: usize,
        obj_index: usize,
        w: u32,
        h: u32,
    ) -> mpsc::Receiver<Option<PdfImagePixels>> {
        let (tx, rx) = mpsc::channel();
        let _ = self
            .tx
            .send((0, 0, WorkerMsg::ImagePixels(doc_id, page, obj_index, w, h, tx)));
        rx
    }

    /// 请求文档级检索（异步、低优先级；全页文本层扫描，命中上限 [`SEARCH_MAX_HITS`]）。
    /// 返回接收结果的通道；文档不存在 → `None`。
    pub fn search(&self, doc_id: DocId, needle: String) -> mpsc::Receiver<Option<PdfSearchResults>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::Search(doc_id, needle, tx)));
        rx
    }

    // ---- Phase A：写回 / 页操作 / 注释读回（同步等 worker 结果；FPDF 调用全在 worker 线程）----

    /// 把一批注释写回 PDF 文件并落盘（worker 内全流程；成功后更新内存镜像）。
    /// 成功回传实际写入的注释条数；`output_path` 为用户当前打开的源文件路径。
    pub fn annotate(
        &self,
        doc_id: DocId,
        items: Vec<AnnotateItem>,
        output_path: PathBuf,
    ) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::Annotate(doc_id, items, output_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 在 `at_index` 处插入一页空白页并落盘（`at_index` 0 基；= 当前页数即末尾追加；
    /// viewer 侧「当前页后插入」传 `page_index + 1`）。成功回传**新页数**。
    pub fn insert_blank_page(
        &self,
        doc_id: DocId,
        at_index: usize,
        width_pt: f32,
        height_pt: f32,
        output_path: PathBuf,
    ) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::InsertBlankPage(doc_id, at_index, width_pt, height_pt, output_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 删除第 `index` 页并落盘（仅剩 1 页 / 越界 → Err）。成功回传**新页数**。
    pub fn delete_page(&self, doc_id: DocId, index: usize, output_path: PathBuf) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::DeletePage(doc_id, index, output_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 把第 `index` 页提取成独立 PDF 文件（`out_path` 原子替换）。
    pub fn extract_page_to(
        &self,
        doc_id: DocId,
        index: usize,
        out_path: PathBuf,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::ExtractPage(doc_id, index, out_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 显式把当前内存 bytes 重新编码落盘到 `path`（无内容修改的另存/刷新）。
    pub fn save(&self, doc_id: DocId, path: PathBuf) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::Save(doc_id, path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    // ---- 待保存模型（P1）：内存工作副本操作——只改 stores、不碰磁盘 ----

    /// **内存级**插入空白页（`at_index` 0 基；= 当前页数即末尾追加）：只改 worker 内
    /// 内存工作副本并回传 [`MemOpResult`]（before/after/count），**不写盘**。
    /// viewer 拿到 `after` 做内存级重载（页面操作待存语义）。
    pub fn mem_insert_blank_page(
        &self,
        doc_id: DocId,
        at_index: usize,
        width_pt: f32,
        height_pt: f32,
    ) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MemOpInsert(doc_id, at_index, width_pt, height_pt, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **内存级**删除第 `index` 页（仅剩 1 页 / 越界 → Err）：只改 stores、不写盘。
    pub fn mem_delete_page(&self, doc_id: DocId, index: usize) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MemOpDelete(doc_id, index, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **内存级**重排页序（`new_order` 为 0 基**全排列**；不合法 → Err 且文档不变、无副作用）。
    /// 只改 stores、不写盘。成功回传 [`MemOpResult`]（count = 重排后页数，不变）。
    pub fn mem_reorder_pages(&self, doc_id: DocId, new_order: Vec<usize>) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MemOpReorder(doc_id, new_order, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **内存级**批量删除页面（`pages` 为 0 基待删页数组；逐页校验 + 去重 + 保留至少 1 页，
    /// 非法 → Err）。只改 stores、不写盘。成功回传 [`MemOpResult`]（count = 删除后页数）。
    pub fn mem_batch_delete_pages(&self, doc_id: DocId, pages: Vec<usize>) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MemOpBatchDelete(doc_id, pages, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **提取多页到新文件**（`pages` 为 0 基待提取页数组，可任意顺序/非连续；非法 →
    /// Err 零副作用）。**不改变当前文档**（副本语义）。成功回传写入文件的页数。
    pub fn export_pages_to(
        &self,
        doc_id: DocId,
        pages: Vec<usize>,
        out_path: PathBuf,
    ) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::ExportPages(doc_id, pages, out_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **拆分区间到新文件并移除**（`range = (start, end)` 含端点 0 基；非法 → Err 零副作用）。
    /// 成功回传 `(新文件页数, MemOpResult)`（`mem.count` = 移除区间后的当前页数）。
    pub fn split_to_file(
        &self,
        doc_id: DocId,
        range: (usize, usize),
        out_path: PathBuf,
    ) -> Result<(usize, MemOpResult)> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::SplitToFile(doc_id, range, out_path, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// **合并另一 PDF 到当前（追加）**（`src_bytes` 为已走读端大小上界的合并源字节，
    /// 非法/失败 → Err 且当前文档不变）。成功回传 [`MemOpResult`]（`count` = 新总页数）。
    pub fn merge_from_bytes(
        &self,
        doc_id: DocId,
        src_bytes: Vec<u8>,
    ) -> Result<MemOpResult> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MergeFromBytes(doc_id, src_bytes, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 用给定 bytes 直接替换 stores 内存镜像（撤销「页面操作」用；零 pdfium 操作）。
    pub fn mem_restore_bytes(&self, doc_id: DocId, bytes: Vec<u8>) -> Result<usize> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::MemRestoreBytes(doc_id, bytes, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 读回 stores 当前内存镜像（`None` = 文档不存在；测试 MD5 / 撤销快照 / 另存用）。
    pub fn mem_bytes(&self, doc_id: DocId) -> Option<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::MemGetBytes(doc_id, tx)));
        rx.recv().ok().flatten()
    }

    /// stores 物化状态探针（T3 DIAG 只读，测试用；**不触发物化**）：`None` = 尚未物化
    /// （True memory 断言：打开即省一份源字节驻留）；`Some(len)` = 已物化字节长。
    pub fn mem_state(&self, doc_id: DocId) -> Option<usize> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send((0, 0, WorkerMsg::MemState(doc_id, tx)));
        rx.recv().ok().flatten()
    }

    /// 读回某页的注释摘要（类型 + quad 点坐标；只读 pdfium-render 高层 API）。
    /// 供测试断言与 Phase B 展示已写回注释。
    pub fn annotations_for_page(
        &self,
        doc_id: DocId,
        page_index: usize,
    ) -> Result<Vec<AnnotationInfo>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((0, 0, WorkerMsg::ReadAnnotations(doc_id, page_index, tx)))
            .context("pdfium 工作线程已退出")?;
        rx.recv().context("pdfium 工作线程已退出")?
    }

    /// 工作线程主体（进程内唯一）：绑定 pdfium 一次 → 循环处理命令。
    fn run(rx: mpsc::Receiver<(u64, u64, WorkerMsg)>, dll: PathBuf) {
        let bindings = match Pdfium::bind_to_library(&dll) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pdfium bind failed: {e}");
                return;
            }
        };
        let pdfium = Pdfium::new(bindings);
        // 借出进程级 bindings（同一原生库句柄）：写回/页操作的低层 FPDF_* 调用都走它，
        // 与 PdfDocument 渲染共用同一 pdfium 实例（单工作线程不变式，见模块头注释）。
        let fpdf: &dyn PdfiumLibraryBindings = PdfiumLibraryBindingsAccessor::bindings(&pdfium);
        // 文档存活期 = `pdfium` 存活期；所有 `PdfDocument<'a>` 都借用上面的 `pdfium`。
        let mut docs: HashMap<DocId, PdfDocument<'_>> = HashMap::new();
        // 各文档在内存中的源 bytes（与 `docs` 同生命周期）：写回/提取/另存时把 bytes
        // 载入**自有 raw 句柄**做修改再落盘，不动正在渲染的 PdfDocument；落盘后同步镜像，
        // 保证后续操作基于最新内容（重开显示由 viewer 的 Close→Open 负责）。
        let mut stores: HashMap<DocId, Arc<Vec<u8>>> = HashMap::new();
        // 本地优先级队列：按类分桶（render > mid > thumb > 其余），桶内 FIFO——
        // **Render 永不排在 Text 之后**（滚动画帧的栅格化优先于懒加载的文本提取），
        // 同优先级保持到达顺序。替代原「单队 + 每轮 `iter().position()+remove(i)`」的
        // O(n) 扫描抽取；调度语义（严格类优先级 + 同类 FIFO）与原实现完全一致。
        // item = `(enqueue-n_gen, WorkerMsg)`（L2.2）：`n_gen` 供出队时与文档级 `navigate_gen`
        // 镜像比对，render 类消息 `n_gen < navigate_gen[doc]` → drop 不渲（只渲最新落点）。
        let mut render_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut mid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut thumb_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut grid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut rest_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        // 在途的全文档检索（分片续跑）：worker 无渲染/占位积压时每轮扫一片（几页/几 ms），
        // 保证检索期间到达的 Render/RenderTile 可以插队先跑——否则一次全量扫描会冻结所有
        // 瓦片/封面（87 页文本 PDF 实测检索 ~440ms，期间渲染被整段堵住 → 随机页先模糊）。
        let mut pending_search: Option<PendingSearch> = None;
        // 纸墨重着色参数（**按文档**键控；`Option`：None = 直通）。每次栅格化 return 前经
        // apply_recolor 就地重着色；SetRecolor 在收到当下按 doc_id 写入（见下方 sweep 的
        // match）。进程级单 worker 多阅读器共享 → 绝不能退化成单槽全局值（会跨窗口互相
        // 覆盖、把错误纸墨渲到对方文档上）。
        let mut recolor: HashMap<DocId, Option<RecolorParams>> = HashMap::new();
        // **重着色世代号**（按文档）：view 是世代**唯一真源**（`set_recolor` 实际变更时 +1），
        // worker 只**采纳**消息携带的世代值（`max(当前, 消息)`），绝不自行递增——修评审 BLOCK：
        // 开档时序 worker 接线前的多次 `set_recolor` 经 `apply_loaded` 补推折叠成一次
        // `SetRecolor`，若 worker 侧自行计数会永远落后 view（旧色结果全部被 view 世代守卫
        // 丢弃 → CPU 回退路径整页空白）。worker 出队渲染时把该世代回填进结果
        // （[`RenderResult::recolor_epoch`] / [`TileResult::recolor_epoch`]），view 据此丢弃
        // 「切档后仍在途的旧世代结果」——修 W1：SetRecolor/RecolorRefresh 与在途渲染走两条
        // mpsc 无总体次序，旧色结果晚到会覆盖新参数槽位。缺省=0。
        let mut recolor_epoch: HashMap<DocId, u64> = HashMap::new();
        // **导航世代**（按文档，L1/L2）：view 是**唯一真源**（大跳 `nav_gen++` 时经
        // [`WorkerMsg::Navigate`] 推送），worker 只**采纳**（`max(当前, 消息)`）为镜像、绝不
        // 自行递增。出队 render 类消息时若 `enqueue-n_gen < navigate_gen[doc]` → 直接 drop 不渲
        // （只渲染最新落点；快速连续跳的中间页不再烧 pdfium 带宽）；在飞项渲完由 view 侧
        // 清 pending（rx drop）丢弃。缺省=0 → 常规非大跳滚动零影响。
        let mut navigate_gen: HashMap<DocId, u64> = HashMap::new();
        // **缩放世代**（按文档，Z2）：view 是**唯一真源**（每次缩放新档 `zoom_epoch++` 时经
        // [`WorkerMsg::SetZoomEpoch`] 推送），worker 只**采纳**（`max(当前, 消息)`）为镜像、绝不
        // 自行递增。出队 render 类消息时若 `enqueue-zoom < zoom_epoch_mirror[doc]` → 直接 drop
        // 不渲（中间档 cover/瓦片/preview 留在队列也是白烧 pdfium 带宽；最终档 stamp==镜像必过）。
        // 与 nav_gen 独立（滚动/大跳时代不变）；缺省=0 → 常规非缩放零影响。控制消息
        // （Text/Search 等非 render）stamp 恒 0 → 不受本 drop 影响。
        let mut zoom_epoch_mirror: HashMap<DocId, u64> = HashMap::new();
        // 未着色原图缓存（**按文档**键控，条目见 [`RawEntry`]）：切档（`SetRecolor`）只对
        // 缓存内该文档的原图重跑 apply_recolor 现算新着色副本回传刷新可见区——不失效缓存
        // 槽、不重置门控、不重调 pdfium 栅格（根治「夜间档加载慢」）。与 recolor 同生命周期，
        // 关闭文档时一并清理（防 doc_id 复用残留旧原图）。
        let mut raw_cache: HashMap<DocId, Vec<RawEntry>> = HashMap::new();
        // 网格视图缩略图缓存（**按文档**键控、独立预算 LRU，见 [`GridEntry`]）：与
        // raw_cache（封面/瓦片原图）、阅读器 THUMB/MID/cover 缓存**完全隔离**——网格是
        // 桌面背景信道，budget 独立、逐出只动本方条目，绝不挤占/污染阅读器驻留。条目 =
        // **最终着色** BGRA（`render_grid` 复用 thumb 的着色出口；**仅 CPU 模式**——GPU
        // 门控下 `finalize_render` 直传 raw/luma、着色在绘制时现算，缓存存的是原图/luma）；
        // `SetRecolor`/`Close` 时随文档清空（CPU 模式保缓存按当前离线着色、不残留旧色，
        // 格卡廉价可秒级重渲；GPU 模式清亦无害）。
        // `last_used` = 全局单调序列号（LRU 裁决只在本文档缓存内比较，全局递增即可）。
        let mut grid_cache: HashMap<DocId, Vec<GridEntry>> = HashMap::new();
        let mut grid_seq: u64 = 0;
        // 网格**带宽预熟**状态（本线，M1+T6）：`Some` = 已开启当前文档「以视口中心页为
        // 核心的水平带宽」预熟（见 [`WorkerMsg::SetGridPrewarm`]）。只在「四桶全空 + 无在途
        // 检索/刷新」时推进（run 循环 5.5 步），从中心页起双向交替外扩：近带（[`GridPrewarmGeom::near`]）
        // 全清+低清双档、中带（window 内 beyond near）仅低清、更远不预熟；窗口刷齐即自然
        // idle（`None`），**不再整本推进**——长文档预熟有界、LRU 滑动窗抖动/永不完成根治。
        // 文档关闭/切换/recolor 时由下方各分支重置游标或清除；滚动重定中心由 view 推送新
        // 几何（从新中心重扫，旧窗口已缓存页快速跳过）。预熟不经 view round-trip（无 tx
        // 回传），结果只落 worker 缓存。近带 ⊆ view 可视带+保留窗不变式见
        // [`config::pdf::GRID_NBAND_ROWS`] 注释。
        let mut grid_prewarm: Option<GridPrewarm> = None;
        // 在途的重着色刷新分帧状态（见 [`PendingRefresh`]）：切档只刷可见集、逐批回传。
        let mut pending_refresh: Option<PendingRefresh> = None;
        // 各文档最近一次**已完成**刷新所用的参数：worker 端同参短路基准（同参不再重复回传）。
        let mut last_refreshed: HashMap<DocId, Option<RecolorParams>> = HashMap::new();
        loop {
            // 现积压一次性分桶入队（每桶 FIFO；跨类优先序由下方「先取高优先桶」体现）。
            // `SetRecolor`/`RecolorRefresh`/`Navigate` 是控制消息：**当下应用**，不入桶——
            // 保证 Navigate 早于同批次的任何渲染出队生效（view 先发 Navigate 推进世代、
            // 再发携带新世代的渲染请求，经单通道 FIFO 保序 → 旧世代请求出队时被 drop）。
            while let Ok(item) = rx.try_recv() {
                let (n_gen, z_gen, msg) = item;
                match msg {
                    WorkerMsg::SetRecolor(doc_id, params, epoch) => {
                        // 只更新参数（重着色刷新走 RecolorRefresh）；同参写入幂等，跳过重复。
                        // BLOCK：worker 采纳 view 消息携带的世代（view 唯一真源；接线前多次
                        // 变更经补推折叠成一次消息，靠携带的最终世代对齐），不自行递增。
                        let cur = recolor_epoch.get(&doc_id).copied().unwrap_or(0);
                        recolor_epoch.insert(doc_id, cur.max(epoch));
                        recolor.insert(doc_id, params);
                        // 网格缓存存的是「最终着色」位图（仅 CPU 回退模式；GPU 门控下存
                        // raw/luma、着色在绘制时现算）：切档即旧色失效 → 整文档清空（格卡
                        // 廉价可秒级重渲，避免夜/日档切换残留旧色卡；GPU 模式清亦无害）。
                        grid_cache.remove(&doc_id);
                        // 缓存已清 → 预熟从新几何中心重扫（旧游标会漏扫；新色按当前带宽窗口
                        // 以新色重熟，窗口内已缓存页跳过）。
                        if let Some(p) = grid_prewarm.as_mut() {
                            if p.doc_id == doc_id {
                                p.reach = 0;
                                p.dir = -1;
                                p.done = false;
                            }
                        }
                    }
                    WorkerMsg::SetGridPrewarm(doc_id, full_card, quick_card, scale_factor, geom) => {
                        // 预熟开启/目标/几何更新（「网格在用」显式信号）：进入网格 / 卡宽变化 /
                        // 滚动重定中心时 view 推送全清 + 低清两档目标宽 + 带宽几何（center/window/
                        // near）。分三种：
                        //  - 跨桶（full 档目标分桶变了）→ 目标作废，从新中心重扫新桶；
                        //  - 同桶但几何变了（滚动/重定中心）→ 更新几何、扫描从新中心重起（旧窗口
                        //    已缓存页快速跳过，代价只限新入带页）；
                        //  - 同桶同几何重复推送 → 幂等：**不重置扫描进度**（推进中的预熟不打断，
                        //    视口在同窗口内微移时不重扫白耗 idle 片）。
                        let same_doc = grid_prewarm.as_ref().is_some_and(|p| p.doc_id == doc_id);
                        let cur_bucket = grid_prewarm
                            .as_ref()
                            .map(|p| grid_bucket_width(p.full_card * p.scale_factor.max(1.0)));
                        let new_bucket = grid_bucket_width(full_card * scale_factor.max(1.0));
                        match grid_prewarm.as_mut() {
                            Some(p) if same_doc => {
                                let geom_changed = p.geom != geom;
                                if cur_bucket != Some(new_bucket) || geom_changed {
                                    p.full_card = full_card;
                                    p.quick_card = quick_card;
                                    p.scale_factor = scale_factor;
                                    p.geom = geom;
                                    p.reach = 0;
                                    p.dir = -1;
                                    p.done = false;
                                }
                            }
                            _ => {
                                grid_prewarm = Some(GridPrewarm {
                                    doc_id,
                                    full_card,
                                    quick_card,
                                    scale_factor,
                                    geom,
                                    reach: 0,
                                    dir: -1,
                                    done: false,
                                });
                            }
                        }
                    }
                    WorkerMsg::RecolorRefresh(doc_id, keys, tx) => {
                        // 圈定可见集 → 挂分帧状态（首轮循环的 4.5 步开始逐批回传）。
                        start_recolor_refresh(
                            &mut pending_refresh,
                            &mut last_refreshed,
                            &recolor,
                            doc_id,
                            keys,
                            tx,
                        );
                    }
                    WorkerMsg::Navigate(doc_id, nav) => {
                        // L1/L2：worker 采纳 view 消息携带的导航世代（view 唯一真源），
                        // 不自行递增；`max` 兜底消息乱序（单通道 FIFO 下本不会发生）。
                        let cur = navigate_gen.get(&doc_id).copied().unwrap_or(0);
                        navigate_gen.insert(doc_id, cur.max(nav));
                    }
                    WorkerMsg::SetZoomEpoch(doc_id, zoom) => {
                        // Z2：worker 采纳 view 消息携带的缩放世代（view 唯一真源），
                        // 不自行递增；`max` 兜底消息乱序（单通道 FIFO 下本不会发生）。
                        let cur = zoom_epoch_mirror.get(&doc_id).copied().unwrap_or(0);
                        zoom_epoch_mirror.insert(doc_id, cur.max(zoom));
                    }
                    other => {
                        enqueue_msg(
                            (n_gen, z_gen, other),
                            &mut render_q,
                            &mut mid_q,
                            &mut thumb_q,
                            &mut grid_q,
                            &mut rest_q,
                        )
                    }
                }
            }
            // 1) 渲染永远最优先：全尺寸 Render / RenderTile（瓦片与整页同为视口内容），
            //    即使渲染消息排在检索/文本之后到，也会先从队列里抽出来执行。
            if let Some((n_gen, z_gen, msg)) = render_q.pop_front() {
                let doc = match &msg {
                    WorkerMsg::Render(d, ..)
                    | WorkerMsg::RenderTile(d, ..)
                    | WorkerMsg::RenderPreview(d, ..) => Some(*d),
                    _ => unreachable!("render_q 只入 Render/RenderTile/RenderPreview"),
                };
                // L2.3：出队 drop——`enqueue-n_gen < navigate_gen[doc]` → 直接丢弃不渲。
                // 快速连续跳时旧世代请求（中间落点页）不再白白消耗 pdfium 带宽；`continue`
                // 跳过渲染与回传（rx 由 view 侧大跳清 pending 时已 drop，静默无副作用）。
                // Z2：同理 `enqueue-zoom < zoom_epoch_mirror[doc]` → drop（缩放**中间档**的
                // cover/瓦片/preview 留在队列也是白烧带宽；最终档请求 stamp==镜像必过）。
                if let Some(d) = doc {
                    let cur = navigate_gen.get(&d).copied().unwrap_or(0);
                    if n_gen < cur {
                        continue;
                    }
                    let zcur = zoom_epoch_mirror.get(&d).copied().unwrap_or(0);
                    if z_gen < zcur {
                        continue;
                    }
                }
                match msg {
                     WorkerMsg::Render(doc_id, req, tx) => {
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_cache = raw_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| render_page(doc, req, p, doc_cache, RawKind::Cover, gpu))
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    WorkerMsg::RenderPreview(doc_id, req, tx) => {
                        // F4 廊封面预览：同 Render 几何与优先级，但 kind=Mid → `finalize_render`
                        // 的 Mid/Thumb 分支：**不入 raw 缓存**（瞬态底图，切档刷新无消费方）。
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_cache = raw_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| render_page(doc, req, p, doc_cache, RawKind::Mid, gpu))
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    WorkerMsg::RenderTile(doc_id, req, tx) => {
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_cache = raw_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| render_tile(doc, req, p, doc_cache, gpu))
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("render_q 只入 Render/RenderTile/RenderPreview"),
                }
                continue;
            }
            // 2) LOD 占位：Mid（中清）> Thumb（缩略），低于渲染、高于其余。
            if let Some((n_gen, z_gen, msg)) = mid_q.pop_front() {
                let doc = match &msg {
                    WorkerMsg::Mid(d, ..) => Some(*d),
                    _ => unreachable!("mid_q 只入 Mid"),
                };
                // L2.3 + Z2：同 render_q 的世代/缩放 drop（旧世代中清占位不渲）。
                if let Some(d) = doc {
                    let cur = navigate_gen.get(&d).copied().unwrap_or(0);
                    if n_gen < cur {
                        continue;
                    }
                    let zcur = zoom_epoch_mirror.get(&d).copied().unwrap_or(0);
                    if z_gen < zcur {
                        continue;
                    }
                }
                match msg {
                    WorkerMsg::Mid(doc_id, page, sf, tx) => {
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_cache = raw_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| render_mid(doc, page, sf, p, doc_cache, gpu))
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("mid_q 只入 Mid"),
                }
                continue;
            }
            if let Some((n_gen, z_gen, msg)) = thumb_q.pop_front() {
                let doc = match &msg {
                    WorkerMsg::Thumb(d, ..) => Some(*d),
                    _ => unreachable!("thumb_q 只入 Thumb"),
                };
                // L2.3 + Z2：同 render_q 的世代/缩放 drop（旧世代缩略图占位不渲）。
                if let Some(d) = doc {
                    let cur = navigate_gen.get(&d).copied().unwrap_or(0);
                    if n_gen < cur {
                        continue;
                    }
                    let zcur = zoom_epoch_mirror.get(&d).copied().unwrap_or(0);
                    if z_gen < zcur {
                        continue;
                    }
                }
                match msg {
                    WorkerMsg::Thumb(doc_id, page, tx) => {
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_cache = raw_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| render_thumb(doc, page, p, doc_cache, gpu))
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("thumb_q 只入 Thumb"),
                }
                continue;
            }
            // 3) 其余 FIFO：Open / Close / Text / Search / 写回·页操作·注释读回。
            //    Search 只登记成分片状态、不在此阻塞；写回类（Annotate/Insert/Delete/Extract/Save）
            //    与注释读回属低频结构操作，放这里不抢栅格化（Render 总是最先出队）。
            //    L2.3 只对 render 类消息做世代 drop；rest 类（Text/Search/Open/写回…）与导航
            //    世代无关，`n_gen` 恒忽略（zoom stamp 同理，非渲染类不参与世代比对）。
            if let Some((_gen, _z, msg)) = rest_q.pop_front() {
                match msg {
                    WorkerMsg::Open(doc_id, bytes, tx) => {
                        let t0 = std::time::Instant::now();
                        // T3 延迟克隆：打开不再镜像源字节（stores 留空，省一份整本驻留）；
                        // 首写/首读经 [`materialize_stores`] 从 live 渲染 doc 惰性物化。
                        // 原始 bytes 移给渲染 doc（pdfium 内部 source_byte_buffer 持有）。
                        let result = match pdfium.load_pdf_from_byte_vec(bytes, None) {
                            Ok(doc) => {
                                let t1 = std::time::Instant::now();
                                // 优先用 FPDF_GetPageSizeByIndexF（页树 MediaBox，**不加载页对象**，
                                // 免去逐页 Resources/字体解析，实测 87 页 284ms → ~5ms）；个别 PDF
                                // 该 API 失败时回退逐页加载量尺寸。
                                let dims: Vec<(f32, f32)> = match doc.pages().page_sizes() {
                                    Ok(sizes) => sizes
                                        .into_iter()
                                        .map(|r| (r.width().value, r.height().value))
                                        .collect(),
                                    Err(_) => doc
                                        .pages()
                                        .iter()
                                        .map(|p| (p.width().value, p.height().value))
                                        .collect(),
                                };
                                let t2 = std::time::Instant::now();
                                let outlines = extract_outline(&doc);
                                let t3 = std::time::Instant::now();
                                if timing() {
                                    eprintln!(
                                        "[{:.1}ms] [OPEN-STRUCT] load={:.1}ms dims({})={:.1}ms outline={:.1}ms total={:.1}ms",
                                        crate::pdf::timing_ms(),
                                        t1.duration_since(t0).as_secs_f64() * 1e3,
                                        dims.len(),
                                        t2.duration_since(t1).as_secs_f64() * 1e3,
                                        t3.duration_since(t2).as_secs_f64() * 1e3,
                                        t3.duration_since(t0).as_secs_f64() * 1e3,
                                    );
                                }
                                docs.insert(doc_id, doc);
                                Ok((dims, outlines))
                            }
                            Err(e) => Err(anyhow::anyhow!("PDF 解析失败：{e}")),
                        };
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Close(doc_id) => {
                        docs.remove(&doc_id);
                        stores.remove(&doc_id);
                        // 重着色按文档键控：关闭即清理（防 doc_id 复用残留旧参数）。
                        recolor.remove(&doc_id);
                        // WARN-1（评审）：世代 map 随文档清理，防长期开闭 PDF 后无界增长
                        // （DocId 单调不复用，残留键永不复用）。
                        recolor_epoch.remove(&doc_id);
                        // L1/L2：导航世代 map 随文档清理（同 recolor_epoch 防残留键增长，
                        // 且防 doc_id 复用读到旧世代把新渲请求全部误 drop）。
                        navigate_gen.remove(&doc_id);
                        // Z2：缩放世代 map 随文档清理（同 navigate_gen 防残留键增长）。
                        zoom_epoch_mirror.remove(&doc_id);
                        last_refreshed.remove(&doc_id);
                        // 未着色原图缓存随文档一起清理（释放 CPU 内存，防跨文档残留）。
                        raw_cache.remove(&doc_id);
                        // 网格视图缩略图缓存随文档一起清理（释放内存、防 doc_id 复用残留）。
                        grid_cache.remove(&doc_id);
                        // 预熟随文档关闭一并停（防 doc_id 复用残留旧预熟游标）。
                        if grid_prewarm.as_ref().is_some_and(|p| p.doc_id == doc_id) {
                            grid_prewarm = None;
                        }
                        // 关闭本应在途的重着色刷新（残留分帧状态会在空 raw 上空转）。
                        if pending_refresh.as_ref().is_some_and(|pr| pr.doc_id == doc_id) {
                            pending_refresh = None;
                        }
                    }
                    WorkerMsg::Text(doc_id, page, tx) => {
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| extract_page_text(doc, page));
                        let _ = tx.send(result);
                    }
                    WorkerMsg::PageImages(doc_id, page, tx) => {
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| extract_page_images(doc, page));
                        let _ = tx.send(result);
                    }
                    WorkerMsg::ImagePixels(doc_id, page, obj_index, w, h, tx) => {
                        let result = docs.get(&doc_id).and_then(|doc| {
                            extract_image_pixels(doc, page, obj_index, w, h)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Search(doc_id, needle, tx) => {
                        if needle.is_empty() {
                            let _ = tx.send(Some(PdfSearchResults {
                                hits: Vec::new(),
                                truncated: false,
                            }));
                        } else if let Some(doc) = docs.get(&doc_id) {
                            // 只登记成分片状态：后续每轮在「步骤 4」分片执行，不整段占用 worker
                            pending_search = Some(PendingSearch {
                                doc_id,
                                needle: needle.chars().collect(),
                                next_page: 0,
                                page_count: doc.pages().len().max(0) as usize,
                                hits: Vec::new(),
                                truncated: false,
                                tx,
                            });
                        } else {
                            let _ = tx.send(None);
                        }
                    }
                    // ---- 写回 / 页操作 / 注释读回（全部 worker 单线程内，见各 handler）----
                    WorkerMsg::Annotate(doc_id, items, out, tx) => {
                        let result = writeback_edit(&mut stores, &docs, fpdf, doc_id, &out, |b, raw| {
                            apply_annotations_raw(b, raw, &items)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::InsertBlankPage(doc_id, at, w, h, out, tx) => {
                        let result = writeback_edit(&mut stores, &docs, fpdf, doc_id, &out, |b, raw| {
                            let page = unsafe { b.FPDFPage_New(raw, at as c_int, w as f64, h as f64) };
                            if page.is_null() {
                                return Err(anyhow!("插入空白页失败（at={at} {w:.1}x{h:.1}pt）"));
                            }
                            // 新页句柄须 Close 释放（同 FPDF_LoadPage 的对称语义）
                            unsafe { b.FPDF_ClosePage(page) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::DeletePage(doc_id, index, out, tx) => {
                        // 越界/单页守卫以**实际待写 bytes**（stores←raw 句柄页数）为准，而非
                        // `docs`（渲染文档可能在连环操作中滞后于 stores，旧计数会误判/漏判）。
                        let result = writeback_edit(&mut stores, &docs, fpdf, doc_id, &out, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            if count <= 1 {
                                return Err(anyhow!("不能删除仅剩的 1 页（当前 {count} 页）"));
                            }
                            if index >= count {
                                return Err(anyhow!("页码越界：删除第 {index} 页（共 {count} 页）"));
                            }
                            unsafe { b.FPDFPage_Delete(raw, index as c_int) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::ExtractPage(doc_id, index, out, tx) => {
                        let result = (|| -> Result<()> {
                            // T3：统一走惰性物化（缺失即从 live doc SaveAsCopy 产出并落 stores）
                            let bytes = materialize_stores(&mut stores, &docs, doc_id)?;
                            unsafe {
                                let raw_src = load_raw_doc(fpdf, &bytes)?;
                                let raw_dst = fpdf.FPDF_CreateNewDocument();
                                if raw_dst.is_null() {
                                    fpdf.FPDF_CloseDocument(raw_src);
                                    return Err(anyhow!("FPDF_CreateNewDocument 失败"));
                                }
                                // pagerange 为 1 基页码串；index=0 表示插入到新文档开头
                                let range = (index + 1).to_string();
                                let ok = fpdf.FPDF_ImportPages(raw_dst, raw_src, &range, 0);
                                if !fpdf.is_true(ok) {
                                    fpdf.FPDF_CloseDocument(raw_dst);
                                    fpdf.FPDF_CloseDocument(raw_src);
                                    return Err(anyhow!("提取第 {index} 页失败"));
                                }
                                let res = save_raw_doc_to_file(fpdf, raw_dst, &out);
                                fpdf.FPDF_CloseDocument(raw_dst);
                                fpdf.FPDF_CloseDocument(raw_src);
                                res
                            }
                        })();
                        let _ = tx.send(result);
                    }
                    WorkerMsg::Save(doc_id, out, tx) => {
                        let result = (|| -> Result<()> {
                            // T3：统一走惰性物化（缺失即从 live doc SaveAsCopy 产出并落 stores）
                            let bytes = materialize_stores(&mut stores, &docs, doc_id)?;
                            unsafe {
                                let raw = load_raw_doc(fpdf, &bytes)?;
                                let res = save_raw_doc_to_file(fpdf, raw, &out);
                                fpdf.FPDF_CloseDocument(raw);
                                res
                            }
                        })();
                        let _ = tx.send(result);
                    }
                    // 待保存模型：内存级页面操作 / 镜像替换——只改 stores，不写盘。
                    WorkerMsg::MemOpInsert(doc_id, at, w, h, tx) => {
                        let result = writeback_edit_mem(&mut stores, &docs, fpdf, doc_id, |b, raw| {
                            let page = unsafe { b.FPDFPage_New(raw, at as c_int, w as f64, h as f64) };
                            if page.is_null() {
                                return Err(anyhow!("插入空白页失败（at={at} {w:.1}x{h:.1}pt）"));
                            }
                            unsafe { b.FPDF_ClosePage(page) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemOpDelete(doc_id, index, tx) => {
                        let result = writeback_edit_mem(&mut stores, &docs, fpdf, doc_id, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            if count <= 1 {
                                return Err(anyhow!("不能删除仅剩的 1 页（当前 {count} 页）"));
                            }
                            if index >= count {
                                return Err(anyhow!("页码越界：删除第 {index} 页（共 {count} 页）"));
                            }
                            unsafe { b.FPDFPage_Delete(raw, index as c_int) };
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemOpReorder(doc_id, new_order, tx) => {
                        let result = reorder_edit_mem(&mut stores, &docs, fpdf, doc_id, new_order);
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemOpBatchDelete(doc_id, pages, tx) => {
                        let result = writeback_edit_mem(&mut stores, &docs, fpdf, doc_id, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            // 逐页校验 + 去重（重复项只删一次，不视作错误）。
                            let mut to_delete: Vec<usize> = Vec::with_capacity(pages.len());
                            for &p in pages.iter() {
                                if p >= count {
                                    return Err(anyhow!("页码越界：批量删除第 {p} 页（共 {count} 页）"));
                                }
                                if !to_delete.contains(&p) {
                                    to_delete.push(p);
                                }
                            }
                            // 保留至少 1 页（对齐单页删除「不能删除仅剩的 1 页」不变式，
                            // 防产出 0 页文档）。
                            if to_delete.len() >= count {
                                return Err(anyhow!("批量删除将删光全部 {count} 页（须保留至少 1 页）"));
                            }
                            // 按索引降序循环删除：先删高位，低位下标不受已删高位位移影响
                            // （升序删会因下标位移错删）。
                            to_delete.sort_unstable_by(|a, b| b.cmp(a));
                            for &p in to_delete.iter() {
                                unsafe { b.FPDFPage_Delete(raw, p as c_int) };
                            }
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::ExportPages(doc_id, pages, out, tx) => {
                        let result = export_pages_to_file(&mut stores, &docs, fpdf, doc_id, &pages, &out);
                        let _ = tx.send(result);
                    }
                    WorkerMsg::SplitToFile(doc_id, (start, end), out, tx) => {
                        // 拆分 = ① 区间提取落盘 + ② 从当前移除（降序删）。整体复用
                        // `writeback_edit_mem` 骨架（before/after/count + stores 更新）：
                        // `written` 由闭包在文件落盘成功后回填，随 `MemOpResult` 一并返回。
                        let mut written: usize = 0;
                        let result = writeback_edit_mem(&mut stores, &docs, fpdf, doc_id, |b, raw| {
                            let count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            // 严格校验：区间有序、端点 < 页数、剩余 ≥1 页（防 0 页文档）。
                            if start > end {
                                return Err(anyhow!("拆分区间起点 {start} 不能大于终点 {end}"));
                            }
                            if end >= count {
                                return Err(
                                    anyhow!("拆分区间越界：{start}..={end}（共 {count} 页）")
                                );
                            }
                            let len = end - start + 1;
                            if len >= count {
                                return Err(anyhow!(
                                    "拆分将移除全部 {count} 页（须保留至少 1 页）"
                                ));
                            }
                            // ① 区间页按原序（升序连续段）提取落盘——先写文件，失败则整个
                            //    操作失败、当前文档不变（原子写盘，目标文件不受影响）。
                            let seq: Vec<usize> = (start..=end).collect();
                            let dst = new_doc_importing_seq(b, raw, &seq)?;
                            match unsafe { save_raw_doc_to_file(b, dst, &out) } {
                                Ok(()) => written = seq.len(),
                                Err(e) => {
                                    unsafe { b.FPDF_CloseDocument(dst) };
                                    return Err(e);
                                }
                            }
                            unsafe { b.FPDF_CloseDocument(dst) };
                            // ② 从当前文档移除该区间：降序删（先删高位，低位下标不受已删
                            //    高位位移影响——升序删会因下标位移错删）。
                            let mut idx = end as c_int;
                            loop {
                                unsafe { b.FPDFPage_Delete(raw, idx) };
                                if idx == start as c_int {
                                    break;
                                }
                                idx -= 1;
                            }
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result.map(|mem| (written, mem)));
                    }
                    WorkerMsg::MergeFromBytes(doc_id, src_bytes, tx) => {
                        // 追加合并复用 `writeback_edit_mem` 骨架（before/after/count + stores
                        // 更新）：闭包内从**字节消息**解析（view 侧读端已走过大小上界
                        // `PDF_MERGE_MAX_SRC_BYTES` 与「合并源过大」文案——worker 不再读文件、
                        // 不再阻塞 pdfium 单工作线程；字节经通道切边，view 侧元数据预检 +
                        // 有界读盘后才发此消息）+ `FPDF_ImportPages` 追加。解析失败快速 Err 护栏
                        // （坏字节 → pdfium 快速失败，不 OOM）。
                        let result = writeback_edit_mem(&mut stores, &docs, fpdf, doc_id, |b, raw| {
                            let src_raw = unsafe { load_raw_doc(b, &src_bytes)? };
                            let src_count = unsafe { b.FPDF_GetPageCount(src_raw) } as usize;
                            if src_count == 0 {
                                unsafe { b.FPDF_CloseDocument(src_raw) };
                                return Err(anyhow!("合并源不含任何页面"));
                            }
                            let dst_count = unsafe { b.FPDF_GetPageCount(raw) } as usize;
                            // 追加：`FPDF_ImportPages` 的 index 是**插入位置**，必须传
                            // 「目标文档当前页数」才能追加到尾部（传 0 = 插到开头）。
                            let range = if src_count == 1 {
                                "1".to_string()
                            } else {
                                format!("1-{src_count}")
                            };
                            let ok = unsafe {
                                b.FPDF_ImportPages(raw, src_raw, &range, dst_count as c_int)
                            };
                            unsafe { b.FPDF_CloseDocument(src_raw) };
                            if !b.is_true(ok) {
                                return Err(anyhow!(
                                    "合并导入失败（FPDF_GetLastError={}）",
                                    fpdf_last_error(b)
                                ));
                            }
                            Ok(unsafe { b.FPDF_GetPageCount(raw) } as usize)
                        });
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemRestoreBytes(doc_id, bytes, tx) => {
                        let result = if stores.contains_key(&doc_id) {
                            stores.insert(doc_id, Arc::new(bytes));
                            Ok(stores.get(&doc_id).map(|b| b.len()).unwrap_or(0))
                        } else {
                            Err(anyhow!("文档 {doc_id} 不存在（无法恢复镜像）"))
                        };
                        let _ = tx.send(result);
                    }
                    WorkerMsg::MemGetBytes(doc_id, tx) => {
                        // T3：惰性物化后读回（None = 文档不存在 / 物化失败）
                        let r = materialize_stores(&mut stores, &docs, doc_id)
                            .ok()
                            .map(|b| b.to_vec());
                        let _ = tx.send(r);
                    }
                    WorkerMsg::MemState(doc_id, tx) => {
                        // T3 DIAG 只读探针：stores 物化状态（不触发物化）。
                        // `None` = 尚未物化（打开即省一份源字节）；`Some(len)` = 已物化字节长。
                        let r = stores.get(&doc_id).map(|b| b.len());
                        let _ = tx.send(r);
                    }
                    WorkerMsg::RawCacheLen(doc_id, tx) => {
                        let n = raw_cache.get(&doc_id).map_or(0, |v| v.len());
                        let _ = tx.send(n);
                    }
                    WorkerMsg::GridCacheLen(doc_id, tx) => {
                        let n = grid_cache.get(&doc_id).map_or(0, |v| v.len());
                        let _ = tx.send(n);
                    }
                    WorkerMsg::GridCacheBuckets(doc_id, tx) => {
                        let entries = grid_cache
                            .get(&doc_id)
                            .map(|v| v.iter().map(|e| (e.page, e.bucket_px)).collect())
                            .unwrap_or_default();
                        let _ = tx.send(entries);
                    }
                    WorkerMsg::NavigateGen(doc_id, tx) => {
                        let n = navigate_gen.get(&doc_id).copied().unwrap_or(0);
                        let _ = tx.send(n);
                    }
                    WorkerMsg::ReadAnnotations(doc_id, page, tx) => {
                        let result = match docs.get(&doc_id) {
                            Some(doc) => read_annotations(doc, page)
                                .ok_or_else(|| anyhow!("文档或第 {page} 页不存在")),
                            None => Err(anyhow!("文档 {doc_id} 不存在")),
                        };
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("rest_q 只入 Open/Close/Text/PageImages/ImagePixels/Search/写回/页操作/注释读回"),
                }
                continue;
            }
            // 3.5) Grid 网格缩略图（背景信道，**最低优先级**）：排序 render > mid > thumb >
            //      rest > grid——仅当 render/mid/thumb 与 rest 类（Open/Close/Text/Search/
            //      写回…）均空时才出队，网格洪流绝不阻塞/饿死阅读器任何既有请求（含文档
            //      开关与文本/检索——rest 类恒先于 grid 出队）。桶内 FIFO + L2.3/Z2 世代
            //      drop（同 Thumb）。
            if let Some((n_gen, z_gen, msg)) = grid_q.pop_front() {
                let doc = match &msg {
                    WorkerMsg::Grid(d, ..) => Some(*d),
                    _ => unreachable!("grid_q 只入 Grid"),
                };
                // L2.3 + Z2：同 Thumb 的世代/缩放 drop（旧世代网格卡不渲）。
                if let Some(d) = doc {
                    let cur = navigate_gen.get(&d).copied().unwrap_or(0);
                    if n_gen < cur {
                        continue;
                    }
                    let zcur = zoom_epoch_mirror.get(&d).copied().unwrap_or(0);
                    if z_gen < zcur {
                        continue;
                    }
                }
                match msg {
                    WorkerMsg::Grid(doc_id, page, card_width, scale_factor, tx) => {
                        let p = recolor.get(&doc_id).copied().flatten();
                        let epoch = recolor_epoch.get(&doc_id).copied().unwrap_or(0); // W1
                        let gpu = crate::recolor::gpu_recolor_gate();
                        let doc_grid_cache = grid_cache.entry(doc_id).or_default();
                        let result = docs
                            .get(&doc_id)
                            .and_then(|doc| {
                                render_grid(
                                    doc,
                                    page,
                                    card_width * scale_factor.max(1.0),
                                    p,
                                    doc_grid_cache,
                                    &mut grid_seq,
                                    gpu,
                                )
                            })
                            .map(|mut r| { r.recolor_epoch = epoch; r }); // W1 世代回填
                        let _ = tx.send(result);
                    }
                    _ => unreachable!("grid_q 只入 Grid"),
                }
                continue;
            }
            // 4) 无渲染/占位/其他积压时，跑一片在途检索再回主循环（新渲染消息可插队）。
            //    每片至多数页/数 ms，因此检索期间的前台渲染冻结有上界，不再拖到全文扫完。
            if let Some(mut ps) = pending_search.take() {
                search_slice(&mut docs, &mut ps);
                if ps.is_done() {
                    let _ = ps.tx.send(Some(PdfSearchResults {
                        hits: ps.hits,
                        truncated: ps.truncated,
                    }));
                } else {
                    pending_search = Some(ps);
                }
                continue;
            }
            // 4.5) 重着色刷新**分帧分批**回传：每轮循环只就地重着色并回传一个
            //      [`RECOLOR_BATCH_BYTES`] 的批次（≥ 1 条，首批 = scope 首=近中心页），随后
            //      `continue` 回主循环——渲染/占位消息可在批次间插队，主线程每帧也只收当前
            //      已到达的批次。全部回传完（keys 空）→ 记录 `last_refreshed`、断通道；
            //      raw 恒为未着色单真值（现算现发，绝不改缓存）。
            if let Some(mut pr) = pending_refresh.take() {
                let mut budget = RECOLOR_BATCH_BYTES as i64;
                let mut batch: Vec<&RawEntry> = Vec::new();
                while let Some(key) = pr.keys.front().copied() {
                    // 查该 key 的缓存原图（已被 FIFO 挤出 → 跳过该 key：view 保持旧着色，
                    // 用户滚回/缩放到该格时走常规渲染按新参数现算，不补回传）。
                    // 【预期行为】48MB 预算紧张（`RAW_CACHE_BYTES`）下「滚出再滚回」的首可视页
                    // 封面恰是最早写入的最旧条目，最易被挤出 → 刷新对该 key 为空跳过 = **部分
                    // 刷新**（回传 ⊆ 仍驻留缓存的可见集子集），靠下一交互重渲恢复——这是预算
                    // 取舍下的预期行为，不是缺陷（WARN-1 注释 + WARN-2 专项测试钉死）。
                    let raw = raw_cache
                        .get(&pr.doc_id)
                        .and_then(|v| {
                            v.iter().find(|e| {
                                e.kind == key.kind
                                    && e.page == key.page
                                    && e.row == key.row
                                    && e.col == key.col
                            })
                        });
                    match raw {
                        Some(raw) => {
                            let cost = raw.bgra.len() as i64;
                            if !batch.is_empty() && cost > budget {
                                break; // 本批预算满（≥ 1 条已装入）
                            }
                            budget -= cost;
                            batch.push(raw);
                            pr.keys.pop_front();
                        }
                        None => {
                            pr.keys.pop_front();
                        }
                    }
                }
                for en in recolored_entries(&batch, pr.params, crate::recolor::gpu_recolor_gate()) {
                    let _ = pr.tx.send(en);
                }
                if pr.keys.is_empty() {
                    // 本批把剩余全发完 → 记录完成参数（同参短路基准），tx 随后 drop 断通道。
                    last_refreshed.insert(pr.doc_id, pr.params);
                } else {
                    pending_refresh = Some(pr); // 下轮循环继续下一批
                }
                continue;
            }
            // 5.5) 网格**带宽预熟**（本线，M1+T6）：四桶全空 + 无在途检索/刷新（走到这里即
            //      证 render/mid/thumb/rest/grid 全空 + pending_search/pending_refresh 无，
            //      因为任一非空都会在更早步骤 `continue` 走掉）且预熟启用未完成时，推进
            //      **以视口中心页为核心的水平带宽**预熟：近带（[`GridPrewarmGeom::near`]，
            //      完整覆盖 view 可视带+保留窗）全清 + 低清双档、中带（[`GridPrewarmGeom::window`]
            //      内 beyond near）仅低清、更远不预熟——`render_grid` 直接算好写进
            //      `grid_cache`（不经 view round-trip、不重开 pdfium；结果只落 worker `grid_cache`）。
            //      扫描**从中心页起双向交替外扩**（`reach`/`dir`），窗口内已缓存页快速跳过、缺失
            //      桶逐页栅格；每渲 1 页（至多两档）`continue` 回主循环查队列——任何真实消息
            //      （含阅读模式渲染/Open/Close）到达即被抢占，预熟只在整机空闲时推进，**绝不
            //      耽误阅读模式**（渲染/占位/rest 恒先出队）。窗口刷齐（额定最大页距扫完）→
            //      **自然 idle 停**（`grid_prewarm = None`），不再推进「全整本」——长文档预熟
            //      有界常驻几十 MB，整本淡扫 242p 超 GRID_CACHE_FULL_DOC_MAX 256MB cap 触发的
            //      LRU 滑动窗/永不完成负资产根治。滚动把中心重定 → view 推送新几何 → 新窗口
            //      从新中心重扫（旧窗口已缓存页快速跳过）。预设目标宽 = [`WorkerMsg::SetGridPrewarm`]
            //      携带的**全清 + 低清两档**：滚动风暴走低清桶、idle 升级走全清桶——任一方向/
            //      滚动状态命中缓存才真零闪（预熟只补「缓存里还没有对应桶」的页，已缓存跳过，
            //      不自重复渲染）。先把预熟字段按值取出（避免 `grid_prewarm` 借用跨 render_grid
            //      与状态清/改）。
            let prewarm_snap = grid_prewarm.as_ref().map(|p| {
                (
                    p.doc_id,
                    p.full_card,
                    p.quick_card,
                    p.scale_factor,
                    p.geom,
                    p.reach,
                    p.dir,
                    p.done,
                )
            });
            if let Some((pre_doc_id, pre_full, pre_quick, pre_scale, geom, reach, dir, done)) =
                prewarm_snap
            {
                let Some(page_count) = docs.get(&pre_doc_id).map(|d| d.pages().len() as usize) else {
                    // 预熟文档已关闭（正常应经 Close 清 prewarm，这里兜底防 race）。
                    grid_prewarm = None;
                    continue;
                };
                if !done {
                    let scale = pre_scale.max(1.0);
                    let full_px = pre_full * scale;
                    let quick_px = pre_quick * scale;
                    let full_bucket = grid_bucket_width(full_px);
                    let quick_bucket = grid_bucket_width(quick_px);
                    let (lo, hi) = geom.window;
                    let (near_lo, near_hi) = geom.near;
                    let center = geom.center_page.min(page_count.saturating_sub(1));
                    // 额定最大页距：窗口两端到中心最远距离（`reach` 越过即双侧必已扫遍 → 刷齐）。
                    let max_reach = hi.saturating_sub(center).max(center.saturating_sub(lo));
                    let cache = grid_cache.entry(pre_doc_id).or_default();

                    // 从当前扫描游标推进：跳过窗口外 / 已刷齐页，定位本 idle 片待渲页（至多一页）。
                    // `has_full`/`has_quick` 在此页渲染前捕获（本片未渲它；两档都缺失才待渲）。
                    let mut scan = (reach, dir);
                    let mut pending: Option<usize> = None;
                    let mut finish = false;
                    let mut has_full = false;
                    let mut has_quick = false;
                    while pending.is_none() && !finish {
                        let (r, d) = scan;
                        if r > max_reach {
                            finish = true;
                            break;
                        }
                        // 候选页 = center ± r（按当前侧）；r=0 = 中心页本身。
                        let cand = if r == 0 {
                            center
                        } else if d >= 0 {
                            center.saturating_add(r)
                        } else {
                            center.saturating_sub(r)
                        };
                        // 推进扫描：处理完当前侧翻转方向；回同侧时页距 +1（下、上、下…交替外扩）。
                        if d >= 0 {
                            scan = (r, -1);
                        } else {
                            scan = (r + 1, 1);
                        }
                        if cand < lo || cand > hi {
                            continue; // 窗口外（一侧越界；另一侧由 max_reach 覆盖继续扫）
                        }
                        let in_near = cand >= near_lo && cand <= near_hi;
                        has_full = cache.iter().any(|e| e.page == cand && e.bucket_px == full_bucket);
                        has_quick = cache
                            .iter()
                            .any(|e| e.page == cand && e.bucket_px == quick_bucket);
                        // 近带 → 需全清 + 低清双档；中带（window 内 beyond near）→ 仅低清。
                        let need_full = in_near && !has_full;
                        let need_quick = !has_quick;
                        if need_full || need_quick {
                            pending = Some(cand);
                            break;
                        }
                        // 该页所需桶已齐 → 继续推进扫描。
                    }

                    if finish {
                        // 窗口刷齐 → 自然 idle（不再整本推进）。
                        grid_prewarm = None;
                        continue;
                    }
                    if let Some(cand) = pending {
                        let p = recolor.get(&pre_doc_id).copied().flatten();
                        let gpu = crate::recolor::gpu_recolor_gate();
                        if let Some(doc) = docs.get(&pre_doc_id) {
                            let in_near = cand >= near_lo && cand <= near_hi;
                            // 近带 → 全清 + 低清双档；中带 → 仅低清（与 view 风暴/idle 双档
                            // 同口径：「中带仅低清」保证中带页不占全清内存——内存验证见
                            // `grid_band_residency_near_full_mid_low_far_none`）。若与全清同桶
                            // （极窄退化档）第二次 render_grid 命中缓存零成本。
                            if in_near && !has_full {
                                render_grid(doc, cand, full_px, p, cache, &mut grid_seq, gpu);
                            }
                            if !has_quick {
                                render_grid(doc, cand, quick_px, p, cache, &mut grid_seq, gpu);
                            }
                        }
                        // 无论 render 成功与否都推进扫描：失败（页无法渲染）不卡死预熟循环。
                        if let Some(pre) = grid_prewarm.as_mut() {
                            if pre.doc_id == pre_doc_id {
                                pre.reach = scan.0;
                                pre.dir = scan.1;
                            }
                        }
                        continue; // 回主循环查队列（真实消息到达即抢占）
                    }
                    // 理论不可达（while 循环要么 finish 要么定位 pending）：兜底置 idle。
                    grid_prewarm = None;
                    continue;
                } else {
                    // 已刷齐 → 停（自然 idle；滚动重新定心推新几何时重新开启）。
                    grid_prewarm = None;
                    continue;
                }
            }
            // 5) 真空闲：阻塞等新消息（只有在既无排队消息、无在途检索/刷新时才真阻塞）。
            //    `SetRecolor`/`RecolorRefresh`/`Navigate` 与上方 sweep 走同一拦截：收到当下应用，
            //    不入桶（否则经 enqueue_msg 落到 rest_q 会撞 `_ => unreachable!` 把工作线程打死）。
            match rx.recv() {
                Ok((_gen, _z, WorkerMsg::SetRecolor(doc_id, params, epoch))) => {
                    // BLOCK：与上方 sweep 同一「采纳 view 世代」语义（见该分支注释）。
                    let cur = recolor_epoch.get(&doc_id).copied().unwrap_or(0);
                    recolor_epoch.insert(doc_id, cur.max(epoch));
                    recolor.insert(doc_id, params);
                    // 同 sweep：网格缓存存最终着色位图，切档即旧色失效 → 整套清空。
                    grid_cache.remove(&doc_id);
                    // 同 sweep：缓存已清 → 预熟从新几何中心重扫（当前带宽窗口内已缓存页跳过）。
                    if let Some(p) = grid_prewarm.as_mut() {
                        if p.doc_id == doc_id {
                            p.reach = 0;
                            p.dir = -1;
                            p.done = false;
                        }
                    }
                }
                Ok((
                    _gen,
                    _z,
                    WorkerMsg::SetGridPrewarm(doc_id, full_card, quick_card, scale_factor, geom),
                )) => {
                    // 与上方 sweep 同一「预熟开启/目标/几何更新」语义（view 唯一真源）：
                    // 跨桶或几何变 → 更新目标 + 从新中心重扫；同桶同几何重复推送幂等不打断。
                    let same_doc = grid_prewarm.as_ref().is_some_and(|p| p.doc_id == doc_id);
                    let cur_bucket = grid_prewarm
                        .as_ref()
                        .map(|p| grid_bucket_width(p.full_card * p.scale_factor.max(1.0)));
                    let new_bucket = grid_bucket_width(full_card * scale_factor.max(1.0));
                    match grid_prewarm.as_mut() {
                        Some(p) if same_doc => {
                            let geom_changed = p.geom != geom;
                            if cur_bucket != Some(new_bucket) || geom_changed {
                                p.full_card = full_card;
                                p.quick_card = quick_card;
                                p.scale_factor = scale_factor;
                                p.geom = geom;
                                p.reach = 0;
                                p.dir = -1;
                                p.done = false;
                            }
                        }
                        _ => {
                            grid_prewarm = Some(GridPrewarm {
                                doc_id,
                                full_card,
                                quick_card,
                                scale_factor,
                                geom,
                                reach: 0,
                                dir: -1,
                                done: false,
                            });
                        }
                    }
                }
                Ok((_gen, _z, WorkerMsg::RecolorRefresh(doc_id, keys, tx))) => {
                    start_recolor_refresh(
                        &mut pending_refresh,
                        &mut last_refreshed,
                        &recolor,
                        doc_id,
                        keys,
                        tx,
                    );
                }
                Ok((_gen, _z, WorkerMsg::Navigate(doc_id, nav))) => {
                    // L1/L2：与上方 sweep 同一「采纳 view 导航世代」语义（view 唯一真源）。
                    let cur = navigate_gen.get(&doc_id).copied().unwrap_or(0);
                    navigate_gen.insert(doc_id, cur.max(nav));
                }
                Ok((_gen, _z, WorkerMsg::SetZoomEpoch(doc_id, zoom))) => {
                    // Z2：与上方 sweep 同一「采纳 view 缩放世代」语义（view 唯一真源）。
                    let cur = zoom_epoch_mirror.get(&doc_id).copied().unwrap_or(0);
                    zoom_epoch_mirror.insert(doc_id, cur.max(zoom));
                }
                Ok((n_gen, z_gen, msg)) => {
                    enqueue_msg(
                        (n_gen, z_gen, msg),
                        &mut render_q,
                        &mut mid_q,
                        &mut thumb_q,
                        &mut grid_q,
                        &mut rest_q,
                    );
                }
                Err(_) => break, // 所有发送端已关闭（全部阅读器已释放）
            }
        }
    }
}

/// 把工作消息按优先级类分桶（render > mid > thumb > rest > grid），桶内保持到达顺序（FIFO）。
/// 分桶后各桶互不交错：Render/Mid/Thumb/rest 的严格类优先级 + 同类 FIFO 与原「单队 +
/// position/remove 扫描」的调度语义一致，只是每轮取舍从 O(n) 扫描降为 O(1) 取桶头；
/// **Grid（网格缩略图）为新增的最低背景信道**（在 rest 之下，仅当 render/mid/thumb/rest
/// 均空时才被出队，见 run 循环 3.5）——rest 类（Open/Close/Text/Search/写回）的调度
/// 语义不因 Grid 而改变、恒先于网格卡执行。
/// `(n_gen, msg)` = L2.2 通道包装（view 发送时打戳 enqueue-n_gen；出队时 render 类按世代 drop）。
fn enqueue_msg(
    (n_gen, z_gen, msg): (u64, u64, WorkerMsg),
    render_q: &mut VecDeque<(u64, u64, WorkerMsg)>,
    mid_q: &mut VecDeque<(u64, u64, WorkerMsg)>,
    thumb_q: &mut VecDeque<(u64, u64, WorkerMsg)>,
    grid_q: &mut VecDeque<(u64, u64, WorkerMsg)>,
    rest_q: &mut VecDeque<(u64, u64, WorkerMsg)>,
) {
    match msg {
        WorkerMsg::Render(..) | WorkerMsg::RenderTile(..) | WorkerMsg::RenderPreview(..) => {
            render_q.push_back((n_gen, z_gen, msg))
        }
        WorkerMsg::Mid(..) => mid_q.push_back((n_gen, z_gen, msg)),
        WorkerMsg::Thumb(..) => thumb_q.push_back((n_gen, z_gen, msg)),
        // Grid 网格缩略图是**背景信道**：独立桶（优先级 ≤ Thumb），只在更高优先桶空时出队。
        WorkerMsg::Grid(..) => grid_q.push_back((n_gen, z_gen, msg)),
        _ => rest_q.push_back((n_gen, z_gen, msg)),
    }
}

/// 取 pdfium 最近一次错误码的简短描述（诊断用）。
fn fpdf_last_error(bindings: &dyn PdfiumLibraryBindings) -> String {
    let code = unsafe { bindings.FPDF_GetLastError() };
    match code {
        0 => "未知".into(),
        v => format!("0x{v:x}"),
    }
}

/// 全局自增文档 ID（进程级原子）。
fn next_doc_id() -> DocId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests;
