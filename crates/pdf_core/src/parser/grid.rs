//! 网格视图（Grid 档）缩略图通道簇：独立预算 LRU 缓存（[`GridEntry`] / [`grid_cache_insert`] /
//! [`grid_cache_evict`] / [`grid_cache_budget`]）+ 带宽预熟状态（[`GridPrewarm`]）+
//! 渲染入口（[`render_grid`]）+ 分桶（[`grid_bucket_width`]）。
//!
//! 与阅读器 THUMB/MID/raw/cover 缓存**完全隔离**：独立预算 LRU（config::pdf::GRID_CACHE_BYTES）
//! + 背景信道调度（优先级 ≤ Thumb，见 enqueue_msg 与 run 循环 grid_q 段）→ 网格模式绝不
//!
//! 抢/不逐出阅读器驻留。产物与 Thumb 同构（RenderResult，view 直接可画、按页落槽）。
//!
//! 跨簇可见性（模块重组）：根 `run` 读写 [`GridPrewarm`] / [`GridEntry`] 字段并调用
//! [`render_grid`] / [`grid_bucket_width`] → 结构+字段与渲染入口升 `pub(crate)`；测试经根
//! 重导出引用 [`grid_render_count`] / [`grid_cache_insert`] / [`grid_cache_evict`]。

use std::sync::atomic::{AtomicU64, Ordering};

use pdfium_render::prelude::*;

use crate::recolor::RecolorParams;

use super::{DocId, GridPrewarmGeom, RawKind, RenderRequest, RenderResult, render_page};

/// 网格缩略图渲染计数探针（测试用）：每次「未命中缓存 → 真调 pdfium 栅格」自增，
/// 供「同页同分桶二次请求不重栅」的红测断言（命中路径不触碰本计数）。
static GRID_RENDERS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(crate) fn grid_render_count() -> u64 {
    GRID_RENDERS.load(Ordering::Relaxed)
}

/// 网格**带宽预熟**状态载荷（本线）：`grid_prewarm` 的状态载荷。语义 = 「以视口中心页为
/// 核心的水平带宽」——近带（[`GridPrewarmGeom::near`]，完整覆盖 view 可视带+保留窗）全清 +
/// 低清双档、中带（[`GridPrewarmGeom::window`] \ near）仅低清、更远不预熟。`reach`/`dir` =
/// **外向扫描游标**：从 `geom.center_page` 起双向交替外扩（下、上、下、上…），窗口内页
/// 逐页按近/中选桶栅格；窗口刷齐（额定最大页距 [`Self::max_reach_hint`]（由 5.5 步按
/// window 计算）扫完）即自然 idle（`grid_prewarm = None`），**不再推进「全整本」**——长
/// 文档预熟有界常驻几十 MB，整本 LRU 滑动窗抖动/永不完成根治。`full_card`/`quick_card`/
/// `scale_factor` = 预熟目标（来自 view 进入网格/卡宽变化/滚动重定中心时推送的
/// [`super::WorkerMsg::SetGridPrewarm`]，全清 + 低清两档——`render_grid` 前乘成目标像素宽）。
///
/// 跨簇可见性（模块重组）：根 `run` 读写全部字段 → 结构+字段升 `pub(crate)`。
pub(crate) struct GridPrewarm {
    /// 预熟文档。
    pub(crate) doc_id: DocId,
    /// 全清档目标卡宽（逻辑 px；×`scale_factor` = 目标像素宽，与 `request_grid` 同口径）。
    pub(crate) full_card: f32,
    /// 低清快速档目标卡宽（逻辑 px；view 已按 `grid_quick_target_px` 换算出 ≤全清的宽）。
    pub(crate) quick_card: f32,
    /// 显示 DPI（≥1.0 口径沿用 `view` 侧 `scale_factor.max(1.0)`）。
    pub(crate) scale_factor: f32,
    /// 几何窗口 + 中心（view 每次重定中心随 SetGridPrewarm 推送）。
    pub(crate) geom: GridPrewarmGeom,
    /// **外向扫描游标**：距 `geom.center_page` 的页距（0 = 中心页本身；每处理完一页按
    /// [`Self::dir`] 推进）。
    pub(crate) reach: usize,
    /// 当前扫描侧：+1 = 向下（center+reach）、−1 = 向上（center−reach）；处理完当前页翻转
    /// （下、上、下…交替外扩，两侧同步前进）。
    pub(crate) dir: i64,
    /// 窗口已刷齐（额定页距扫完、窗口内无可预熟页）→ 停（自然 idle）；新几何推送时复位。
    pub(crate) done: bool,
}

/// 网格缩略图缓存条目（**最终着色** BGRA；切档由 run 循环清整文档缓存，见 SetRecolor）。
/// key = `(page, bucket_px)`；`last_used` 为全局单调序列号（LRU 裁决在本文档缓存内比较）。
///
/// 跨簇可见性（模块重组）：根 `run`（GridCacheBuckets 读回）读取字段 → 结构+字段升 `pub(crate)`。
pub(crate) struct GridEntry {
    pub(crate) page: usize,
    /// 量化后目标宽（缓存 key 维度；`render_grid` 入口先分桶再查/渲）。
    pub(crate) bucket_px: u32,
    /// 实际位图宽（scale clamp 到 ≤1.0 后可能略小于 bucket_px；回落显示用宽）。
    pub(crate) width: u32,
    /// 实际位图高（按页纵横比）。
    pub(crate) height: u32,
    /// 是否 R8 单通道 luma（同 [`RenderResult::luma`]；view 侧按此选包装路径）。
    pub(crate) luma: bool,
    pub(crate) bgra: Vec<u8>,
    pub(crate) last_used: u64,
}

impl GridEntry {
    /// 跨可见性（模块重组）：`parser/tests.rs` 引用（缓存预算账目求和）→ 升 `pub(crate)`。
    pub(crate) fn bytes(&self) -> u64 {
        self.bgra.len() as u64
    }
}

/// 目标宽量化分桶：向 [`config::pdf::GRID_BUCKET_PX`] 取整（round，≥1 桶）。同一桶内
/// 相邻自然宽度共用同一缓存项（resize 抖动/DPI 微差复用），保证 → 渲染/缓存 key 稳定。
/// 与 `render_scale`/`page_bitmap_size` 的「px = pt × scale」姿势一致——分桶落在 px 侧。
/// 乘法用 saturating（防御病态超大输入在 debug 整数溢出 panic；`q` 有 f32 round 上限，
/// 极端 f32 值 cast 后 saturating 兜底到 `u32::MAX`，不会 wrap 成小桶）。
pub fn grid_bucket_width(target_px: f32) -> u32 {
    let bucket = config::pdf::GRID_BUCKET_PX.max(1);
    let b = bucket as f32;
    let q = (target_px / b).round().max(1.0) as u32;
    q.saturating_mul(bucket)
}

/// 网格缓存写入 + 预算逐出（纯函数，渲染与测试共用）：同 key（页 + 分桶宽）已有 →
/// 更新内容并 bump last_used（返回 `false`，调用方仍用本轮结果作出口）；否则插入并返回
/// `true`。超预算时回收「最久未用」（最小 last_used）——LRU 天然贴近「距视口中心近的
/// 先被请求 → 最近被用」的 PREFETCH_COVER 距离语义（远端卡久不用先被逐出）。
pub(crate) fn grid_cache_insert(cache: &mut Vec<GridEntry>, entry: GridEntry, budget: u64) -> bool {
    if let Some(e) = cache
        .iter_mut()
        .find(|e| e.page == entry.page && e.bucket_px == entry.bucket_px)
    {
        *e = entry;
        grid_cache_evict(cache, budget);
        return false;
    }
    cache.push(entry);
    grid_cache_evict(cache, budget);
    true
}

/// 预算收敛（LRU）：超支时移除最久未用条目，直到回落到预算内；**至少保留 1 条**
/// （刚写入/最近用者随取随用）。**单条超预算口径**：`bytes > budget` 的单条（病态超大
/// 卡位图）不逐出自己——预算语义实为「budget 内尽量多驻留 + 恒保 1 条」，单条超预算
/// 时缓存退化为「该单条独占」而非空缓存（避免「写入即逐出 → 每次请求都重栅」的抖动）。
/// 网格卡宽有 `scale ≤ 1.0` 与 [`pixel_area_ok`] 面积闸兜底，单条尺寸天然有界，
/// 无需额外 per-entry 加帽。
pub(crate) fn grid_cache_evict(cache: &mut Vec<GridEntry>, budget: u64) {
    let mut over: i64 = cache.iter().map(GridEntry::bytes).sum::<u64>() as i64 - budget as i64;
    while over > 0 && cache.len() > 1 {
        let oldest = cache
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(i, _)| i)
            .expect("cache.len() > 1 保证非空");
        over -= cache.remove(oldest).bytes() as i64;
    }
}

/// 网格缓存预算（**动态整本**，本批「当前文档不逐出」杠杆）：= min(整本估算 = `页数 ×
/// 单卡实际字节 × 5/4`，[`config::pdf::GRID_CACHE_FULL_DOC_MAX`] 256MB 单文档上界)，下限
/// [`config::pdf::GRID_CACHE_BYTES`]（32MB 既有兜底）。单卡实际字节取「缓存内最大条目」
/// 与「刚渲染条目」的较大者（同文档同分桶近等尺寸；`scale_factor`>1 时卡面积按平方放大
/// ——固定 512KB/卡估算会在 DPI2 下低估 4×、把整本误逐出，必须用真实字节）。`×5/4` 给
/// 「全清 + 低清」两档并存留头：每页 ≈ 全清 1× + 低清 1/4（预熟两档都写缓存，见 5.5 步）。
/// 效果：常规打开的文档整本（两档）落在预算内 → `grid_cache_insert/evict` 的 LRU **不触发**
/// （预算内不逐出、滚动任意方向回看零重渲、空闲预熟整本可补齐）；仅超长文档/病态大卡
/// 超上界时才走既有 LRU 逐最旧。`page_count` = 当前文档页数、`cur_bytes` = 刚渲染单卡字节。
fn grid_cache_budget(cache: &[GridEntry], page_count: u64, cur_bytes: u64) -> u64 {
    let per_card = cache.iter().map(GridEntry::bytes).max().unwrap_or(0).max(cur_bytes);
    let full_doc_estimate = page_count
        .saturating_mul(per_card.saturating_mul(5) / 4)
        .min(config::pdf::GRID_CACHE_FULL_DOC_MAX);
    full_doc_estimate.max(config::pdf::GRID_CACHE_BYTES)
}

/// 在工作线程内：按目标卡宽渲染某页的**网格缩略图**（独立缓存 + 背景信道）。
///
/// - `target_px` = 目标像素宽（= 目标卡宽逻辑 px × DPI，调用方先乘好）。
/// - 先量化分桶，命中独立缓存 → 直接回（**不重栅**、只 bump LRU 时间戳）；未命中 →
///   复用 [`render_page`] 姿势按 `bucket_px / 页宽` 缩放渲染（不同页纵横比 → 高按比例），
///
/// 产物为与 Thumb 同类型的 `RenderResult`（着色出口 = `RawKind::Thumb` 语义：**不入 raw
///   缓存**、仅 CPU 模式应用当前重着色；GPU 门控直传 raw/luma、着色在绘制时现算），
///   随后入独立网格缓存（预算 LRU，切档已清整文档）。
///
/// 跨簇可见性（模块重组）：根 `run`（Grid 出口与 5.5 预熟）调用 → 升 `pub(crate)`。
pub(crate) fn render_grid(
    doc: &PdfDocument<'_>,
    page_index: usize,
    target_px: f32,
    recolor: Option<RecolorParams>,
    cache: &mut Vec<GridEntry>,
    seq: &mut u64,
    gpu: bool,
) -> Option<RenderResult> {
    let bucket_px = grid_bucket_width(target_px);
    // 命中：返回缓存（最终着色）副本，不触碰 pdfium、不计渲染。
    if let Some(e) = cache
        .iter_mut()
        .find(|e| e.page == page_index && e.bucket_px == bucket_px)
    {
        e.last_used = *seq;
        *seq += 1;
        return Some(RenderResult {
            bgra: e.bgra.clone(),
            width: e.width,
            height: e.height,
            recolor_epoch: 0, // W1：由 run 循环按 doc 世代回填
            luma: e.luma,
        });
    }
    // 未命中：按桶渲染（`&mut vec![]` = 临时空 raw 缓存——`render_page` 对 Thumb 语义
    // 不入 raw 缓存、不触碰该实参，仅占位口径一致）。
    let page = doc.pages().get(page_index as i32).ok()?;
    let pw = page.width().value.max(1.0);
    let scale = (bucket_px as f32 / pw).clamp(0.1, 1.0);
    let result = render_page(
        doc,
        RenderRequest {
            page: page_index,
            scale,
        },
        recolor,
        &mut vec![],
        RawKind::Thumb,
        gpu,
    )?;
    GRID_RENDERS.fetch_add(1, Ordering::Relaxed);
    let entry = GridEntry {
        page: page_index,
        bucket_px,
        width: result.width,
        height: result.height,
        luma: result.luma,
        bgra: result.bgra.clone(),
        last_used: *seq,
    };
    *seq += 1;
    let budget = grid_cache_budget(cache, doc.pages().len() as u64, entry.bytes());
    grid_cache_insert(cache, entry, budget);
    Some(result)
}
