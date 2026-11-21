//! 渲染簇：统一着色出口（[`finalize_render`]）+ `render_page / render_tile / render_mid /
//! render_thumb` + 重着色刷新（[`recolored_entries`] / [`PendingRefresh`] /
//! [`start_recolor_refresh`] / [`RECOLOR_BATCH_BYTES`]）+ worker 侧计时探针 [`timing`]。
//!
//! 跨簇可见性（模块重组）：根 `run` 调用 `render_page/render_tile/render_mid/render_thumb/
//! recolored_entries/start_recolor_refresh` 与读取 `PendingRefresh` 字段、`RECOLOR_BATCH_BYTES`、
//! `timing`（Open 计时）→ 以上升 `pub(crate)`；`render_page` 亦被 `grid::render_grid` 复用。
//! `RawEntry`/`cache_raw`/`raw_cache_budget`（`cache` 簇）与交换数据型（`types` 簇）经根重导出。

// 渲染内核逐参数传参 → 放行
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;

use pdfium_render::prelude::*;

use crate::recolor::{RecolorParams, apply_recolor};

use super::{
    DocId, RawEntry, RawKind, RecolorEntry, RecolorKey, RenderRequest, RenderResult,
    TileRequest, TileResult, cache_raw, raw_cache_budget,
};

/// 是否开启 worker 渲染计时日志（`VUICK_PDF_TIMING=1`）：打印每次瓦片/整页渲染的
/// get_page 与 pdfium 光栅化耗时，便于在真实慢 PDF 上定位瓶颈（worker 侧）。
///
/// 跨簇可见性（模块重组）：根 `run` 的 Open 计时分支调用 → 升 `pub(crate)`。
pub(crate) fn timing() -> bool {
    static EN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EN.get_or_init(|| {
        std::env::var_os("VUICK_PDF_TIMING").is_some_and(|v| v == "1" || v == "true")
    })
}

/// 栅格化出口统一姿势：把未着色原图存入文档 raw 缓存（同 key 同密度复用），返回**按当前
/// 重着色参数着色后的显示副本**（`None` 直通 = 原图不变）。不触碰缓存内的 raw（单真值）。
/// `gpu` = T4 GPU 重着色门控：开启时**跳过 CPU 逐像素着色且不写入 raw 缓存**，出口即
/// 未着色原图本身（着色改在 GPU 绘制时完成；原图由 view 持有、worker 侧缓存 0 消费者）——
/// 显示路径 CPU 税归零、默认路径缓存冗余归零、无 `bgra.clone()` 瞬态 2× 尖峰；
/// 关闭时写缓存 + 着色副本，行为与既有完全一致（CPU 重着色回退路径）。
///
/// 跨簇可见性（模块重组）：`parser/tests.rs` 直接引用（finalize 单测）→ 升 `pub(crate)`。
pub(crate) fn finalize_render(
    cache: &mut Vec<RawEntry>,
    kind: RawKind,
    page: usize,
    row: u32,
    col: u32,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    full_w: u32,
    full_h: u32,
    scale: f32,
    mut bgra: Vec<u8>,
    recolor: Option<RecolorParams>,
    gpu: bool,
    luma: bool,
) -> Vec<u8> {
    if gpu && recolor.is_some() {
        // 真机诊断 2/4：门控下跳过 CPU 着色直传原图——首个封面/瓦片各打一次（latch）。
        crate::recolor::diag_raw_passthrough(kind);
    }
    if gpu {
        // 内存去冗余（GPU 门控默认路径）：worker 侧 raw 缓存 **0 消费者**——GPU 模式下
        // view 持原图、着色在绘制时 `img().recolor` 现算；切档只改图元参数、不派
        // `RecolorRefresh`（见 `view/open.rs::set_recolor` 的 `if !gpu_recolor` 分支）→
        // 不写缓存（默认路径缓存冗余归零），`bgra` **move 直传**出口（不克隆，消除一次性
        // 2× 尖峰）。条件只取决于 GPU/CPU 门控、不取决于当前 recolor 是否 Some：CPU 模式
        // 即使当前 recolor=None 也应保持缓存（用户随时可开夜间，缓存供
        // `SetRecolor`/`RecolorRefresh` 消费、回看按新参现算）。
        // P2 R8：门控激活（GPU + FullBand + ink）时，出口编码成单通道感知亮度（-4× 无损）——
        // GPU shader 的 FullBand 分支只依赖亮度、不读原色，位图只存 luma 数学无损。
        if luma {
            bgra = crate::recolor::encode_bgra_to_luma(&bgra);
        }
        return bgra;
    }
    // CPU 重着色回退路径：raw 缓存只服务「重着色刷新」的消费方（`RecolorRefresh` 圈定的
    // 可见集 = 在位封面 + 瓦片，见 `current_recolor_scope`）；Mid/Thumb 是快速拖动瞬态占位、
    // **不参与重着色刷新** → 其 raw 入缓存属死重（占用预算挤出封面/瓦片原图、切换时无消费
    // 方）。因此只有 Cover/Tile 的 raw 入缓存——预算语义统一为「封面 + 瓦片原图镜像」
    // （对齐 [`config::pdf::RAW_CACHE_BYTES`] = 高倍可见集的 3×封面 + 瓦片）。
    if matches!(kind, RawKind::Cover | RawKind::Tile) {
        let already = cache.iter().any(|e| {
            e.kind == kind
                && e.page == page
                && e.row == row
                && e.col == col
                && (e.scale - scale).abs() <= 1e-4
                && e.w == w
                && e.h == h
        });
        if already {
            // 已缓存同密度同尺寸原图（重渲同格）：缓存 raw 不重复写入，出口用本轮 raw（内容一致）。
            if let Some(p) = recolor {
                apply_recolor(&mut bgra, p);
            }
            return bgra;
        }
        // 新原图：入缓存 + 出口用克隆着色副本。
        let mut out = bgra.clone();
        if let Some(p) = recolor {
            apply_recolor(&mut out, p);
        }
        cache_raw(
            cache,
            RawEntry {
                kind,
                page,
                row,
                col,
                x0,
                y0,
                w,
                h,
                full_w,
                full_h,
                scale,
                bgra,
            },
            raw_cache_budget(),
        );
        return out;
    }
    // Mid/Thumb：瞬态占位不入缓存，出口 `bgra` move 直用 + 着色副本（免克隆）。
    let mut out = bgra;
    if let Some(p) = recolor {
        apply_recolor(&mut out, p);
    }
    out
}

/// 对一批已缓存**未着色原图**按 `params` 重着色，产出待回传的显示副本（**不触碰缓存 raw**，
/// 每条一次 memcpy + 一次 apply_recolor）。`None`（默认档）直通 = 原图副本。`gpu` = T4 GPU
/// 门控：开启时跳过 CPU 着色（只克隆原图副本，着色移到 GPU 绘制时——切档天然 0 重算）。
/// 纯函数：供工作线程分批回传发帧，亦可直接单测（K27 证据入仓）。
///
/// 跨簇可见性（模块重组）：根 `run` 4.5 重着色刷新调用 → 升 `pub(crate)`。
pub(crate) fn recolored_entries(
    cache: &[&RawEntry],
    params: Option<RecolorParams>,
    gpu: bool,
) -> Vec<RecolorEntry> {
    cache
        .iter()
        .map(|e| {
            if gpu && params.is_some() {
                // 真机诊断 2/4：切档刷新路径同样直传（与 finalize_render 共用 latch）。
                crate::recolor::diag_raw_passthrough(e.kind);
            }
            let mut bgra = e.bgra.clone();
            if !gpu && let Some(p) = params {
                apply_recolor(&mut bgra, p);
            }
            RecolorEntry {
                kind: e.kind,
                page: e.page,
                row: e.row,
                col: e.col,
                x0: e.x0,
                y0: e.y0,
                w: e.w,
                h: e.h,
                full_w: e.full_w,
                full_h: e.full_h,
                scale: e.scale,
                bgra,
            }
        })
        .collect()
}

/// 重着色刷新**单批回传字节预算**：worker 每轮循环至多就地重着色并回传这么多字节的
/// 新着色副本（≥ 1 条），下一轮循环再下一批——把「一次把可见集全压到通道/主线程」收敛
/// 为「按既有泵/收集节奏分帧渐进」；scope 由 view 按近中心排序，首批即首可视页。
///
/// 跨簇可见性（模块重组）：根 `run` 4.5 重着色刷新使用 → 升 `pub(crate)`。
pub(crate) const RECOLOR_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// 在途的重着色刷新分帧状态：`keys` 为待回传的可见集 key 队列（view 按近中心排序，
/// 先入先出 = 先刷新首可视页）。每轮循环只发一批 [`RECOLOR_BATCH_BYTES`]（≥ 1 条），
/// 发完断通道（`tx` drop）→ view 侧 drain 完即清 `pending_recolor`。
/// raw 始终保留为未着色单真值（现算现发，绝不着色缓存本体）。
///
/// 跨簇可见性（模块重组）：根 `run` 逐批读取字段 → 结构+字段升 `pub(crate)`。
pub(crate) struct PendingRefresh {
    pub(crate) doc_id: DocId,
    pub(crate) keys: VecDeque<RecolorKey>,
    pub(crate) tx: mpsc::Sender<RecolorEntry>,
    pub(crate) params: Option<RecolorParams>,
}

/// `RecolorRefresh` 的登记（sweep 与空闲 `rx.recv` 两处共用）：挂到 `pending_refresh`，
/// 主循环随后按批分帧回传。**同参短路（worker 端双保险）**：与上次**已完成**的刷新参数
/// 一致时直接断开通道（不重复回传同样内容）——view 侧同参已短路，此处防重复派发冗余。
///
/// 跨簇可见性（模块重组）：根 `run` 调用 → 升 `pub(crate)`。
pub(crate) fn start_recolor_refresh(
    pending_refresh: &mut Option<PendingRefresh>,
    last_refreshed: &mut HashMap<DocId, Option<RecolorParams>>,
    recolor: &HashMap<DocId, Option<RecolorParams>>,
    doc_id: DocId,
    keys: Vec<RecolorKey>,
    tx: mpsc::Sender<RecolorEntry>,
) {
    let params = recolor.get(&doc_id).copied().flatten();
    if last_refreshed.get(&doc_id).copied().flatten() == params {
        // 同参短路：上次已用同参数完成过刷新 → 无新内容可回传。
        drop(tx);
        return;
    }
    *pending_refresh = Some(PendingRefresh {
        doc_id,
        keys: keys.into(),
        tx,
        params,
    });
}

/// 在工作线程内：把单个页面栅格化，返回**紧凑 BGRA** 像素 + 尺寸。
///
/// 关键优化：pdfium 渲染的 bitmap 原生就是 BGRA（`PdfBitmapFormat::BGRA`，stride 恒为
/// `w*4`——见 `PdfBitmap::preferred_stride_bytes` 对 4 通道格式从不补对齐），
/// 直接 `as_raw_bytes()` 拷出即可，**零像素循环、零字节重排**。旧的
/// `as_image()` → `into_rgba8()` 会对每像素做 BGRA→RGBA 再一次 RGBA→BGRA 的浪费。
///
/// 跨簇可见性（模块重组）：根 `run`（Render/RenderPreview/Mid/Thumb 出口）与
/// `grid::render_grid`（Thumb 语义复用）调用 → 升 `pub(crate)`。
pub(crate) fn render_page(
    doc: &PdfDocument<'_>,
    req: RenderRequest,
    recolor: Option<RecolorParams>,
    cache: &mut Vec<RawEntry>,
    kind: RawKind,
    gpu: bool,
) -> Option<RenderResult> {
    let page = doc.pages().get(req.page as i32).ok()?;
    let (pw, ph) = (page.width().value, page.height().value);
    // 与 view 侧 `layout::page_bitmap_size` 共用同一套取整（含 1:1 稳定化）：位图宽必须
    // 与显示矩形（设备像素取整）逐纹素一致，否则 GPU 双线性重采样发糊。
    let (bw, bh) = crate::pdf::layout::page_bitmap_size((pw, ph), req.scale);
    // 面积闸（安全护栏）：病理性天文尺寸（w×h×4 字节 → 整页位图分配撑爆内存）直接拒绝；
    // A4@4.0 ≈ 8M px 恰在预算（12M px）内不受影响（纯函数口径见 layout::pixel_area_ok）。
    if !crate::pdf::layout::pixel_area_ok(bw, bh) {
        return None;
    }
    let (w, h) = (bw as i32, bh as i32);
    let t1 = std::time::Instant::now();
    let bmp = page.render(w, h, None).ok()?;
    // pdfium-render 的 `PdfBitmap::render()` 用 `PdfBitmapFormat::default()`（= BGRA）建位图，
    // BGRA 的 stride 恒为 `width*4`（无对齐填充，见 preferred_stride_bytes）。
    // `as_raw_bytes()` 直接返回整块原生 BGRA 缓冲 → 主线程 `Frame::from_raw` 直接包裹。
    let bgra = bmp.as_raw_bytes();
    // P2 R8 门控（GPU + FullBand + ink）：出口编码成单通道亮度（-4× 无损）。
    let luma = crate::recolor::luma_gate_active(recolor, gpu);
    // 未着色原图入缓存（单真值）+ 出口着色副本（切档可基于缓存 raw 重着色，不重调 pdfium）。
    let bgra = finalize_render(
        cache,
        kind,
        req.page,
        0,
        0,
        0,
        0,
        w as u32,
        h as u32,
        w as u32,
        h as u32,
        req.scale,
        bgra,
        recolor,
        gpu,
        luma,
    );
    if timing() {
        eprintln!(
            "[{:.1}ms] [PAGE-RENDER] p{} scale={:.2} render={:.3}ms -> {}x{}",
            crate::pdf::timing_ms(),
            req.page,
            req.scale,
            t1.elapsed().as_secs_f64() * 1e3,
            w,
            h,
        );
    }
    Some(RenderResult {
        bgra,
        width: w as u32,
        height: h as u32,
        recolor_epoch: 0, // W1：由 run 循环按 doc 世代回填
        luma,
    })
}

/// 在工作线程内：栅格化某页的一格瓦片（SumatraPDF 式分块渲染的主内容路径）。
///
/// 机制（与 pdfium-render 官方 `set_origin` 条带拼接测试同款）：给瓦片大小的
/// BGRA 缓冲，把页面以**整页位图尺寸**为参考缩放到 `full_w × full_h`，再用
/// 负 `set_origin(-x0, -y0)` 把页面左上角推到缓冲右上角——pdfium 渲染时自动
/// 裁剪到缓冲边界，于是只有 `[x0, x0+w) × [y0, y0+h)` 这一格被画进 ~1MB 的小缓冲。
///
/// 内存：每片恒 ≈ TILE_SIZE² × 4，**与缩放倍数无关**；同一页切成 视口可见 的几片，
/// 不再存在"4× 缩放一页 = 64MB 整页位图"的内存/density 上限墙。
///
/// 跨簇可见性（模块重组）：根 `run`（RenderTile 出口）调用 → 升 `pub(crate)`。
pub(crate) fn render_tile(
    doc: &PdfDocument<'_>,
    req: TileRequest,
    recolor: Option<RecolorParams>,
    cache: &mut Vec<RawEntry>,
    gpu: bool,
) -> Option<TileResult> {
    let t0 = std::time::Instant::now();
    let page = doc.pages().get(req.page as i32).ok()?;
    let t_get = std::time::Instant::now();
    let (pw, ph) = (page.width().value, page.height().value);
    let bmp = crate::pdf::layout::page_bitmap_size((pw, ph), req.scale);
    let (x0, y0, w, h) =
        crate::pdf::layout::tile_rect(req.col, req.row, req.tile_size, config::pdf::TILE_BLEED, bmp)?;
    let mut dst = PdfBitmap::empty(w as i32, h as i32, PdfBitmapFormat::BGRA).ok()?;
    let t1 = std::time::Instant::now();
    page.render_into_bitmap_with_config(
        &mut dst,
        &PdfRenderConfig::new()
            .set_fixed_size(bmp.0 as i32, bmp.1 as i32)
            .set_origin(-(x0 as i32), -(y0 as i32)),
    )
    .ok()?;
    let bgra = dst.as_raw_bytes();
    // P2 R8 门控（GPU + FullBand + ink）：出口编码成单通道亮度（-4× 无损）。
    let luma = crate::recolor::luma_gate_active(recolor, gpu);
    // 未着色原图入缓存（单真值）+ 出口着色副本（切档可基于缓存 raw 重着色，不重调 pdfium）。
    let bgra = finalize_render(
        cache,
        RawKind::Tile,
        req.page,
        req.row,
        req.col,
        x0,
        y0,
        w,
        h,
        bmp.0,
        bmp.1,
        req.scale,
        bgra,
        recolor,
        gpu,
        luma,
    );
    let res = TileResult {
        page: req.page,
        row: req.row,
        col: req.col,
        x0,
        y0,
        w,
        h,
        full_w: bmp.0,
        full_h: bmp.1,
        bgra,
        recolor_epoch: 0, // W1：由 run 循环按 doc 世代回填
        luma,
    };
    if timing() {
        let end = std::time::Instant::now();
        eprintln!(
            "[{:.1}ms] [TILE-RENDER] p{} r{}c{} scale={:.2} get_page={:.3}ms render={:.3}ms",
            crate::pdf::timing_ms(),
            req.page,
            req.row,
            req.col,
            req.scale,
            t_get.duration_since(t0).as_secs_f64() * 1e3,
            end.duration_since(t1).as_secs_f64() * 1e3,
        );
    }
    Some(res)
}

/// 在工作线程内：栅格化某页的 **L0 缩略图**（低分辨率，用于拖动期间占位 / 回翻即时内容）。
///
/// 缩放按目标宽度 [`THUMB_WIDTH`](config::pdf::THUMB_WIDTH) 推导并夹在
/// `[0.1, 1.0]`（防止超大页 / 极小页产生荒谬分辨率）；pdfium 下采样极快（~1ms/张）。
///
/// 跨簇可见性（模块重组）：根 `run`（Thumb 出口）调用 → 升 `pub(crate)`。
pub(crate) fn render_thumb(
    doc: &PdfDocument<'_>,
    page_index: usize,
    recolor: Option<RecolorParams>,
    cache: &mut Vec<RawEntry>,
    gpu: bool,
) -> Option<RenderResult> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let pw = page.width().value.max(1.0);
    let scale = (config::pdf::THUMB_WIDTH / pw).clamp(0.1, 1.0);
    render_page(
        doc,
        RenderRequest {
            page: page_index,
            scale,
        },
        recolor,
        cache,
        RawKind::Thumb,
        gpu,
    )
}

/// 在工作线程内：栅格化某页的 **L0.5 中清页**（~520px × DPI 宽，~5ms/张，用于快速拖动）。
///
/// 缩放按目标宽度 [`MID_WIDTH`](config::pdf::MID_WIDTH) × 显示 `scale_factor` 推导
/// 并夹在 `[0.1, 1.0]`：DPI>1 的屏上目标宽度随之放大，预览不经 GPU 大倍率上采样、保持锐利
/// （避免“字体/图像发糊”）。比全清便宜 ~6×，比 260px 缩略图清晰得多（可辨识标题/段落结构）。
///
/// 跨簇可见性（模块重组）：根 `run`（Mid 出口）调用 → 升 `pub(crate)`。
pub(crate) fn render_mid(
    doc: &PdfDocument<'_>,
    page_index: usize,
    scale_factor: f32,
    recolor: Option<RecolorParams>,
    cache: &mut Vec<RawEntry>,
    gpu: bool,
) -> Option<RenderResult> {
    let page = doc.pages().get(page_index as i32).ok()?;
    let pw = page.width().value.max(1.0);
    let target_w = config::pdf::MID_WIDTH * scale_factor.max(1.0);
    let scale = (target_w / pw).clamp(0.1, 1.0);
    render_page(
        doc,
        RenderRequest {
            page: page_index,
            scale,
        },
        recolor,
        cache,
        RawKind::Mid,
        gpu,
    )
}
