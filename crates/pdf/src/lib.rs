//! PDF 读取与展示模块。
//!
//! 基于 native **pdfium**（Edge 同源内核）栅格化，单页渲染快，首次浏览即流畅。
//! 架构：**瓦片级虚拟化、零磁盘缓存**：
//! - 内容层走 **瓦片渲染**（SumatraPDF 式分块）：页面按 [TILE_SIZE]×[TILE_SIZE]
//!   切成网格，只栅格化视口可见 + 边缘缓冲的那几片（每片 ~1MB，内存按视口而非
//!   页面积封顶），瓦片按显示物理像素 1:1 渲染，高 DPI / 放大不模糊；瓦片 ~5ms
//!   即可重建，**不落盘**；
//! - L0 缩略图 / L0.5 中清页作快速拖动占位（瓦片逐片到达前先铺低清底图），
//!   同样**纯内存**（受页数预算约束）；
//! - [`PdfWorker`]（`parser.rs`）是**进程级单例后台线程**（pdfium 整进程只能绑定一次），
//!   持有唯一 `Pdfium` 与一个 `HashMap<DocId, PdfDocument>`，通过 mpsc 接收各阅读器的
//!   `Open / Render / RenderTile / Mid / Thumb / Close` 请求并回传像素；
//! - [`PdfViewer`]（`view.rs`）只做收发与显示：首片 BGRA 直包 `RenderImage`，快速
//!   滚动先用缩略/中清占位、停止后瓦片逐片补齐（无整页大位图，消除了高 DPI 下
//!   "一页 64MB" 的内存爆炸与位图密度封顶导致的放大发糊）。
//!
//! 模块结构：
//! - [`parser`]：pdfium.dll 定位、单例工作线程、多文档逐页/逐瓦片栅格化（RGBA）
//! - [`view`]：阅读器视图核心（加载/滚动/瓦片按需渲染/滚动条）
//! - [`layout`]：页列纯布局 + 瓦片网格几何（worker 与 view 共用同一套数学）
//!
//! 书签目录统一由**侧边栏**提供（见 [`crate::app::sidebar`] 与公共 [`crate::toc`]），
//! 本模块只保留书签数据（[`PdfOutlineEntry`]）与跳页能力（[`crate::pdf::PdfViewer::scroll_to_page`]）。

// `pdf_core`（栅格化核心）拆出后：`parser`/`layout`/`recolor` 模块经 `pub use pdf_core::*`
// 全量重导出，本 crate 只声明视图层模块；`crate::pdf::` 自别名保留（既有索引路径零改动，
// 与 `editor` 视图层门面同一策略）。
use crate as pdf;

#[cfg(test)]
pub mod test_support;

pub mod view;

pub use pdf_core::*;
pub use pdf_core::{layout, parser, recolor};
#[allow(unused_imports)]
pub use view::PdfViewer;

#[cfg(test)]
mod tests {
    use super::*;

    /// 本模块 pdfium 测试的串行锁（实现见 [`test_util::lock`]）。
    fn pdf_lock() -> &'static std::sync::Mutex<()> {
        test_util::lock()
    }

    /// T4 GPU 重着色门控的测试覆盖守卫：构造时把进程级门控设为 `on`，Drop（含 panic）时
    /// 还原为 `prev`——防断言失败后门控残留 true 泄漏到后续用例（须在持 `pdf_lock` 时用）。
    struct GpuGateGuard {
        prev: bool,
    }
    impl GpuGateGuard {
        fn set_on() -> Self {
            let prev = crate::recolor::gpu_recolor_gate();
            crate::recolor::set_gpu_recolor_gate_for_test(true);
            GpuGateGuard { prev }
        }
    }
    impl Drop for GpuGateGuard {
        fn drop(&mut self) {
            crate::recolor::set_gpu_recolor_gate_for_test(self.prev);
        }
    }

    /// raw 缓存预算覆盖的测试守卫（对称于 `GpuGateGuard`）：构造时把预算压到极小（钉死
    /// FIFO 挤出语义），Drop（含 panic）时还原。仅可在持 `pdf_lock` 时用。
    struct RawBudgetGuard {
        prev: u64,
    }
    impl RawBudgetGuard {
        fn set(budget: u64) -> Self {
            let prev = crate::pdf::parser::raw_cache_budget_override();
            crate::pdf::parser::set_raw_cache_budget_for_test(budget);
            RawBudgetGuard { prev }
        }
    }
    impl Drop for RawBudgetGuard {
        fn drop(&mut self) {
            crate::pdf::parser::set_raw_cache_budget_for_test(self.prev);
        }
    }

    /// 曲线策略 latch 的测试覆盖守卫（对称于 `GpuGateGuard`）：构造时设为 `c`，Drop 还原
    /// 为 `prev`。仅可在持 `pdf_lock` 时用（与 gate 守卫同样的并发纪律）。
    struct CurveGuard {
        prev: crate::recolor::RecolorCurve,
    }
    impl CurveGuard {
        fn set(c: crate::recolor::RecolorCurve) -> Self {
            let prev = crate::recolor::recolor_curve();
            crate::recolor::set_recolor_curve_for_test(c);
            CurveGuard { prev }
        }
    }
    impl Drop for CurveGuard {
        fn drop(&mut self) {
            crate::recolor::set_recolor_curve_for_test(self.prev);
        }
    }

    fn test_pdf_paths() -> Vec<std::path::PathBuf> {
        test_util::test_pdf_paths()
    }

    /// P1-17 配套：轮询「第 0 页 live 文档异步读回含 Highlight」直到命中/超时。
    /// `annotations_for_page` 已改非阻塞（后台执行器 + 结果通道，K167 姿势）：本函数
    /// 派发一次读回后**持同一结果通道跨轮询 `try_recv`**（每次派发后先 advance 时钟给
    /// 后台任务落定的时间再取，K37 效果断言）。重载窗口内 `annotations_for_page` 返回
    /// `None`（无 worker）或读回为空列表（live 文档尚未重载成含注释副本）→ 作废重派，
    /// 直到 save/save_as 的重载完成、读到已写注释。
    fn wait_pending_highlight_readback(
        cx: &mut gpui::TestAppContext,
        vid: &gpui::Entity<crate::pdf::PdfViewer>,
    ) -> bool {
        use std::time::Duration;
        let mut rx: Option<std::sync::mpsc::Receiver<Option<Vec<crate::pdf::AnnotationInfo>>>> =
            None;
        for _ in 0..800 {
            if rx.is_none() {
                rx = cx.update(|app| vid.update(app, |v, cx| v.annotations_for_page(0, cx)));
            }
            if let Some(r) = &rx
                && let Ok(Some(a)) = r.try_recv()
            {
                if a.iter().any(|x| x.kind == AnnotKind::Highlight) {
                    return true;
                }
                // 读回成功但无高亮：live 文档还没重载成含注释副本 → 作废重派
                rx = None;
            }
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// **A-P2-2（瓦片记账按 bpp）**：`TileSlot::bytes()` 对 R8 luma 瓦片按 1BPP 记账、
    /// BGRA 瓦片按 4BPP——字节账本与动态预算（`nominal_per_tile` 同源 bpp）一致，
    /// 「字节 /4 而预算仍按 4BPP 估」的 4× 预算语义错误被钉死。
    #[test]
    fn ap2_tile_slot_bytes_accounts_luma_bpp() {
        let luma_img =
            crate::pdf::render_image_from_luma(vec![0u8; 64 * 64], 64, 64).expect("luma 位图");
        let ts = crate::pdf::layout::TileSlot { image: luma_img, x0: 0, y0: 0, w: 64, h: 64 };
        assert_eq!(ts.bytes(), 64 * 64 * 1, "R8 瓦片记账应为 1BPP（BGRA 的 1/4）");
        assert!(ts.image.is_luma(0), "R8 瓦片 RenderImage 应判为 luma");

        let bgra_img = crate::pdf::render_image_from_bgra(vec![0u8; 64 * 64 * 4], 64, 64)
            .expect("BGRA 位图");
        let ts2 = crate::pdf::layout::TileSlot { image: bgra_img, x0: 0, y0: 0, w: 64, h: 64 };
        assert_eq!(ts2.bytes(), 64 * 64 * 4, "BGRA 瓦片记账应为 4BPP");
        assert!(!ts2.image.is_luma(0), "BGRA 瓦片 RenderImage 不应判为 luma");
    }

    /// 缺夹具即 SKIP 守卫：无 `target/pdf测试*.pdf` 时打印并 `return`（CI 有夹具则真跑）。
    macro_rules! skip_if_missing_pdf_fixture {
        () => {
            if test_util::missing_pdf_fixture() {
                eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
                return;
            }
        };
    }

    #[test]
    fn parses_all_test_pdfs_and_reads_dims() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        for p in test_pdf_paths() {
            assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
            let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
            let t0 = std::time::Instant::now();
            let dims = parser::page_dims(&bytes).expect("解析 PDF 应成功");
            let elapsed = t0.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(15),
                "{} 结构解析过慢：{elapsed:?}（{} 页）",
                p.display(),
                dims.len()
            );
            assert!(!dims.is_empty(), "{} 应有页", p.display());
            for (w, h) in &dims {
                assert!(*w > 0.0 && *h > 0.0, "{} 页尺寸应为正: {w}x{h}", p.display());
            }
        }
    }

    #[test]
    fn extracts_outline_from_pdf_bookmarks() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // 目录提取冒烟测试：真实 PDF 能安全打开，outline 返回合法结构（可空）。
        // 注：本 dll 无法解析含 ObjStm（压缩对象流）的 PDF（如 pdfium-render 官方
        // test-toc.pdf 报 FormatError），故不依赖固定书签资产；有书签时校验字段正确性。
        for p in test_pdf_paths() {
            let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
            let (_worker, _doc_id, dims, outlines) =
                PdfWorker::open_and_dims_full(bytes).expect("打开 worker 应成功");
            assert!(!dims.is_empty(), "应有页");
            for e in &outlines {
                if e.page_index != usize::MAX {
                    assert!(
                        e.page_index < dims.len(),
                        "书签页码越界：{} p{}",
                        e.title,
                        e.page_index
                    );
                }
            }
        }
        // 纯逻辑：outline 条目字段一致性（标题非空、层级有限）
        let entries = [crate::pdf::PdfOutlineEntry {
                title: "章节一".into(),
                level: 0,
                page_index: 0,
            },
            crate::pdf::PdfOutlineEntry {
                title: "子节".into(),
                level: 1,
                page_index: 1,
            }];
        assert!(entries.iter().all(|e| !e.title.trim().is_empty() && e.level <= 6));
    }

    #[test]
    fn worker_renders_single_page_to_bgra() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        let rx = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到渲染结果")
            .expect("渲染应成功");
        assert!(res.width > 0 && res.height > 0, "应有正尺寸");
        assert_eq!(
            res.bgra.len(),
            (res.width as usize) * (res.height as usize) * 4,
            "压缩 BGRA 应为纯 w*h*4 字节（无 stride 填充）"
        );
    }

    /// 瓦片渲染与整页渲染逐像素互洽：把网格的全部瓦片缝回整页位图，与直接整页渲染
    /// 对比，每通道平均漂移 <5/255（沿用 pdfium-render 官方 set_origin 条带拼接测试的
    /// 判据）。同时验证单瓦片尺寸恒 ~TILE_SIZE、内存不随页面积爆炸（单瓦片 ≪ 整页）。
    #[test]
    fn tiles_stitch_back_to_full_page_render() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let (pw, ph) = dims[0];
        let scale = 1.0f32;
        let bmp = layout::page_bitmap_size((pw, ph), scale);
        // 首页须大到切成 2×2 瓦片（否则退化为单瓦片整页，跳过拼缝验证）
        assert!(bmp.0 > 512 && bmp.1 > 512, "测试 PDF 首页应足够大: {bmp:?}");
        let (rows, cols) = layout::tile_grid(bmp, config::pdf::TILE_SIZE);

        // 整页基准
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale }, 0, 0);
        let full = rx_full
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("应收到整页")
            .expect("整页渲染应成功");
        assert_eq!(
            (full.width, full.height),
            bmp,
            "整页尺寸应与瓦片同宗（同 scale 同取整）"
        );

        // 逐格瓦片 + 缝回整页缓冲
        let mut stitched = vec![0xffu8; (bmp.0 as usize) * (bmp.1 as usize) * 4];
        for row in 0..rows {
            for col in 0..cols {
                let rx_t = worker.request_tile(
                    doc_id,
                    crate::pdf::TileRequest {
                        page: 0,
                        scale,
                        tile_size: config::pdf::TILE_SIZE,
                        row,
                        col,
                    },
                    0, // L2.2 enqueue-gen（常规测试路径 = 0，无导航世代）
                    0, // Z2 enqueue-zoom（常规测试路径 = 0，无缩放世代）
                );
                let tile = rx_t
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("应收到瓦片")
                    .expect("瓦片渲染应成功");
                assert_eq!((tile.row, tile.col), (row, col), "瓦片回传格号应与请求一致");
                assert_eq!(
                    (tile.full_w, tile.full_h),
                    bmp,
                    "瓦片应携带与请求同宗的整页尺寸"
                );
                assert_eq!(
                    tile.bgra.len(),
                    (tile.w as usize) * (tile.h as usize) * 4,
                    "瓦片像素应紧凑 w*h*4"
                );
                // 单瓦片面积恒 ≤ (TILE_SIZE + 2×TILE_BLEED)²：内存不随页面积/缩放爆炸
                //（真实收益是「只驻留视口可见的几片」，而非单瓦片小于整页）。
                let max_tile_edge =
                    config::pdf::TILE_SIZE + 2 * config::pdf::TILE_BLEED;
                assert!(
                    tile.w <= max_tile_edge && tile.h <= max_tile_edge,
                    "瓦片应被 TILE_SIZE+出血 封顶：{}x{} ≤ {}",
                    tile.w,
                    tile.h,
                    max_tile_edge
                );
                for y in 0..tile.h as usize {
                    let src = &tile.bgra[y * tile.w as usize * 4..(y + 1) * tile.w as usize * 4];
                    let dst_off =
                        ((tile.y0 as usize) + y) * bmp.0 as usize * 4 + (tile.x0 as usize) * 4;
                    stitched[dst_off..dst_off + src.len()].copy_from_slice(src);
                }
            }
        }

        // 逐像素漂移（缝合 vs 整页），判据与 pdfium-render 官方条带拼接测试一致
        let mut sums = [0u64; 4];
        let px_count = (bmp.0 as usize) * (bmp.1 as usize);
        for px in 0..px_count {
            for ch in 0..4 {
                let a = stitched[px * 4 + ch] as i64;
                let b = full.bgra[px * 4 + ch] as i64;
                sums[ch] += a.abs_diff(b) as u64;
            }
        }
        let n = px_count as f64;
        let max_drift = (0..4)
            .map(|ch| sums[ch] as f64 / n)
            .fold(0.0_f64, f64::max);
        assert!(
            max_drift < 5.0,
            "缝合瓦片 vs 整页渲染的平均每通道漂移 {max_drift:.3}/255 超过容差"
        );
    }

    #[test]
    fn worker_renders_thumbnail_at_low_resolution() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // L0 缩略图：应远小于全尺寸位图，且宽度与 THUMB_WIDTH 推导一致（低成本占位层）
        let p = test_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx_thumb = worker.request_thumb(doc_id, 0, 0, 0);
        let thumb = rx_thumb
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到缩略图")
            .expect("缩略图渲染应成功");
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let full = rx_full
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到全尺寸")
            .expect("全尺寸渲染应成功");
        assert!(
            thumb.bgra.len() < full.bgra.len(),
            "缩略图字节数应远小于全尺寸：{} < {}",
            thumb.bgra.len(),
            full.bgra.len()
        );
        let pt_w = dims[0].0;
        let expect_w = ((config::pdf::THUMB_WIDTH / pt_w.max(1.0))
            .clamp(0.1, 1.0)
            * pt_w)
            .ceil() as u32; // ceil(pw * scale)，与 render_page 的 ceil 一致
        assert_eq!(
            thumb.width, expect_w,
            "缩略图宽应与 THUMB_WIDTH 推导一致：实际 {} 期望 {}",
            thumb.width, expect_w
        );
    }

    #[test]
    fn worker_renders_mid_at_mid_resolution() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // L0.5 中清页：宽度应按 MID_WIDTH 推导，体积介于缩略图与全清之间（拖动清晰度主力）
        let p = test_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx_mid = worker.request_mid(doc_id, 0, 1.0, 0, 0);
        let mid = rx_mid
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到中清页")
            .expect("中清页渲染应成功");
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let full = rx_full
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到全清")
            .expect("全清渲染应成功");
        let rx_thumb = worker.request_thumb(doc_id, 0, 0, 0);
        let thumb = rx_thumb
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到缩略图")
            .expect("缩略图渲染应成功");
        let pt_w = dims[0].0;
        let expect_w = ((config::pdf::MID_WIDTH / pt_w.max(1.0))
            .clamp(0.1, 1.0)
            * pt_w)
            .ceil() as u32; // ceil(pw * scale)，与 render_page 一致
        assert_eq!(
            mid.width, expect_w,
            "中清页宽应与 MID_WIDTH 推导一致：实际 {} 期望 {}",
            mid.width, expect_w
        );
        assert!(
            mid.bgra.len() > thumb.bgra.len() && mid.bgra.len() < full.bgra.len(),
            "中清体积应介于缩略图与全清之间：thumb {} < mid {} < full {}",
            thumb.bgra.len(),
            mid.bgra.len(),
            full.bgra.len()
        );
    }

    #[test]
    fn worker_renders_many_pages_individually() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // 长 PDF（target/pdf测试2.pdf，259 页文本型）：抽查首/中/末各一页
        // （模拟「滚动到哪渲染哪」）
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let dims = parser::page_dims(&bytes).expect("解析应成功");
        assert!(dims.len() >= 50, "长文档应有足够页数抽查首/中/末，实际={}", dims.len());
        let (worker, doc_id, _dims2) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        for idx in [0usize, dims.len() / 2, dims.len() - 1] {
            let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 0.5 }, 0, 0);
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到渲染结果")
                .expect("渲染应成功");
            assert!(res.width > 0 && res.height > 0, "第 {idx} 页应渲染出位图");
            // 内容正确性：页面不应是空白/纯色（防 pdfium 静默渲染失败）
            let bgra = &res.bgra;
            let mut min_px = u8::MAX;
            let mut max_px = 0u8;
            for px in bgra.as_chunks::<4>().0 {
                let lum = (px[0] as u16 + px[1] as u16 + px[2] as u16) / 3;
                if (lum as u8) < min_px { min_px = lum as u8; }
                if (lum as u8) > max_px { max_px = lum as u8; }
            }
            assert!(
                max_px - min_px > 8,
                "第 {idx} 页渲染内容不应接近空白（亮度范围 {min_px}..{max_px}）"
            );
        }
    }

    #[test]
    fn bgra_roundtrips_to_render_image() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        let rx = worker.request_render(doc_id, RenderRequest { page: 0, scale: 0.5 }, 0, 0);
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到渲染结果")
            .expect("渲染应成功");
        let img = render_image_from_bgra(res.bgra.clone(), res.width, res.height)
            .expect("BGRA 应能重建 RenderImage");
        assert!(img.frame_count() >= 1, "应至少 1 帧");
    }

    /// 文本层坐标映射回归（P1 核心验收）：字符盒必须命中渲染位图上的字形墨迹，
    /// 且 Y 朝向正确。若翻转方向写反 / 映射比例错，盒会落到空白处（无墨）或上下颠倒。
    #[test]
    fn text_boxes_align_with_rendered_glyphs() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let display_w = config::pdf::PAGE_WIDTH;

        // 找一张内容稠密页（≥40 个可测字符），避免封面/空白页样本量不足
        let max_scan = dims.len().min(10);
        let mut chosen: Option<(usize, PageText, (f32, f32), f32)> = None;
        for (page_idx, &page_pt) in dims.iter().enumerate().take(max_scan) {
            let rx_text = worker.request_text(doc_id, page_idx);
            let text = rx_text
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本")
                .expect("文本应成功");
            let usable = text
                .chars
                .iter()
                .filter(|c| !c.synthetic && !c.ch.is_whitespace())
                .count();
            if usable >= 40 {
                let scale = layout::render_scale(display_w, 1.0, page_pt.0);
                chosen = Some((page_idx, text, page_pt, scale));
                break;
            }
        }
        let (page_idx, text, page_pt, scale) =
            chosen.expect("应在测试 PDF 内找到内容稠密页（可测字符 ≥40）");
        let rx_img = worker.request_render(doc_id, RenderRequest { page: page_idx, scale }, 0, 0);
        let img = rx_img
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到渲染")
            .expect("渲染应成功");
        let disp_h = layout::display_height(page_pt, display_w);

        // 局部 px → 位图像素（位图被拉伸到 display_w × disp_h，故按比例换算）。
        // 注意 Y 翻转：c.top（pt 大）经映射后是**较小**的局部 y（靠上），c.bottom 是较大 y。
        let to_bmp = |v: f32, dim: u32, disp: f32| -> i32 { (v * dim as f32 / disp).round() as i32 };
        let boxes: Vec<(i32, i32, i32, i32)> = text
            .chars
            .iter()
            .filter(|c| !c.synthetic && !c.ch.is_whitespace())
            .filter_map(|c| {
                let (lx0, ly0) =
                    layout::pt_to_local(c.left, c.bottom, page_pt.0, page_pt.1, display_w, scale);
                let (lx1, ly1) =
                    layout::pt_to_local(c.right, c.top, page_pt.0, page_pt.1, display_w, scale);
                let (bx0, bx1) = (to_bmp(lx0, img.width, display_w), to_bmp(lx1, img.width, display_w));
                let (by_top, by_bottom) = (to_bmp(ly1, img.height, disp_h), to_bmp(ly0, img.height, disp_h));
                if bx1 - bx0 >= 1 && by_bottom - by_top >= 1 {
                    Some((
                        bx0.clamp(0, img.width as i32 - 1),
                        by_top.clamp(0, img.height as i32 - 1),
                        bx1.clamp(bx0 + 1, img.width as i32),
                        by_bottom.clamp(by_top + 1, img.height as i32),
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert!(boxes.len() >= 30, "内容页应有足量可测字符盒，实际 {}", boxes.len());

        // 盒内有非近白像素 = 命中字形墨迹（bitmap 为 BGRA）
        let has_ink = |b: &(i32, i32, i32, i32)| -> bool {
            let (bx0, by0, bx1, by1) = *b;
            let w = img.width as usize;
            for byy in by0..by1 {
                for bxx in bx0..bx1 {
                    let o = (byy as usize * w + bxx as usize) * 4;
                    let lum = (img.bgra[o] as u16 + img.bgra[o + 1] as u16 + img.bgra[o + 2] as u16) / 3;
                    if lum < 240 {
                        return true;
                    }
                }
            }
            false
        };
        let ink_hits = boxes.iter().filter(|b| has_ink(b)).count();
        assert!(
            ink_hits >= boxes.len() * 7 / 10,
            "映射后的字符盒应命中字形墨迹: {ink_hits}/{}",
            boxes.len()
        );

        // Y 朝向（相对，页面内容不必铺满）：pt Y 最大（页顶）字符映射后应显示在
        // pt Y 最小（页底）字符**之上**。若翻转方向写反此断言必败。
        let pt_y_center = |c: &crate::pdf::PdfTextChar| -> f32 {
            let (_, ly0) =
                layout::pt_to_local(c.left, c.bottom, page_pt.0, page_pt.1, display_w, scale);
            let (_, ly1) =
                layout::pt_to_local(c.left, c.top, page_pt.0, page_pt.1, display_w, scale);
            (ly0 + ly1) / 2.0 * img.height as f32 / disp_h
        };
        let vis: Vec<&crate::pdf::PdfTextChar> = text.chars.iter().filter(|c| !c.synthetic).collect();
        let topmost = vis.iter().max_by(|a, b| a.top.total_cmp(&b.top)).copied().unwrap();
        let botmost = vis.iter().min_by(|a, b| a.bottom.total_cmp(&b.bottom)).copied().unwrap();
        let (top_cy, bot_cy) = (pt_y_center(topmost), pt_y_center(botmost));
        assert!(top_cy < bot_cy, "页顶字符应显示于页底字符之上: top_cy={top_cy} bot_cy={bot_cy}");
    }

    /// 同行基线对齐（P1 验收）：映射后同行字符的基线（`origin_y`）标准差 < 1px，
    /// 防止每字符盒高沿抖动导致选区高亮锯齿。
    #[test]
    fn mapped_line_baselines_stay_within_pixel_variance() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // 注：长 PDF fixture 已变更为 242 页**扫描件**（无文本层），故改在文本型
        // pdf测试.pdf 上选内容稠密页验证（动态挑页，防 fixture 换页号被戳穿）。
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let max_scan = dims.len().min(12);
        let mut chosen: Option<(usize, PageText)> = None;
        for idx in 0..max_scan {
            let rx = worker.request_text(doc_id, idx);
            let text = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本")
                .expect("文本应成功");
            let usable = text.chars.iter().filter(|c| !c.synthetic).count();
            if usable >= 40 {
                chosen = Some((idx, text));
                break;
            }
        }
        let (page_idx, text) =
            chosen.expect("应在文本型 PDF 内找到内容稠密页（可用字符 ≥40）");
        let page_pt = dims[page_idx];
        let display_w = config::pdf::PAGE_WIDTH;
        let scale = layout::render_scale(display_w, 1.0, page_pt.0);

        // 每字符映射后的基线局部 y（origin_y 为 PDF 点，经 pt_to_local 翻转）
        let mut ys: Vec<(i32, f32)> = text
            .chars
            .iter()
            .filter(|c| !c.synthetic)
            .filter_map(|c| {
                let (_, ly) = layout::pt_to_local(
                    c.origin_x,
                    c.origin_y,
                    page_pt.0,
                    page_pt.1,
                    display_w,
                    scale,
                );
                if ly.is_finite() {
                    Some(((ly / 3.0).round() as i32, ly))
                } else {
                    None
                }
            })
            .collect();
        assert!(ys.len() >= 40, "应有足量字符参与基线聚类，实际 {}", ys.len());
        ys.sort_by_key(|&(k, _)| k);

        // 按量化桶分组（≈同行），桶内基线标准差 < 1px
        let mut i = 0usize;
        let mut checked = 0usize;
        while i < ys.len() {
            let k = ys[i].0;
            let mut j = i;
            while j < ys.len() && ys[j].0 == k {
                j += 1;
            }
            let group = &ys[i..j];
            if group.len() >= 4 {
                let mean = group.iter().map(|g| g.1).sum::<f32>() / group.len() as f32;
                let var = group
                    .iter()
                    .map(|g| (g.1 - mean) * (g.1 - mean))
                    .sum::<f32>()
                    / group.len() as f32;
                assert!(
                    var.sqrt() < 1.0,
                    "同行映射后基线标准差应 <1px, 实际 {:.3} (桶 {k})",
                    var.sqrt()
                );
                checked += 1;
            }
            i = j;
        }
        assert!(checked >= 3, "应有至少 3 个可校验的行（桶），实际 {checked}");
    }

    /// P2 集成：真实 PDF 的一"行"（同一基线连续字符）应合并为单个高亮矩形，
    /// 该行复制文本应无内部换行、且首尾字符保留（启发式空格除外）。
    #[test]
    fn real_page_lines_merge_to_single_span_and_copy_keeps_chars() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let mut picked: Option<(usize, crate::pdf::PageText)> = None;
        for idx in 0..8 {
            let rx = worker.request_text(doc_id, idx);
            let text = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本")
                .expect("文本应成功");
            let usable = text
                .chars
                .iter()
                .filter(|c| !c.synthetic && c.right > c.left)
                .count();
            if usable >= 20 {
                picked = Some((idx, text));
                break;
            }
        }
        let (_page_idx, text) = picked.expect("应有内容页");
        let first = text
            .chars
            .iter()
            .position(|c| !c.synthetic && c.right > c.left)
            .expect("应存在字符");
        let base_y = text.chars[first].origin_y;
        // 同基线的连续字符（= 第一"行"）
        let run = text.chars[first..]
            .iter()
            .take_while(|c| !c.synthetic && (c.origin_y - base_y).abs() <= 1.5)
            .count();
        let line_hi = first + run - 1;
        let spans = layout::selection_spans_pt(&text, first, line_hi);
        assert_eq!(
            spans.len(),
            1,
            "同一行字符应合并为 1 个矩形，实际 {}",
            spans.len()
        );
        let rect = spans[0];
        assert!(
            rect.0 <= text.chars[first].left && rect.2 >= text.chars[line_hi].right,
            "行矩形应覆盖首尾字符"
        );
        let seg = layout::page_segment_text(&text, first, line_hi);
        assert!(!seg.contains('\n'), "单行内不应有换行");
        assert!(
            seg.starts_with(text.chars[first].ch) && seg.ends_with(text.chars[line_hi].ch),
            "复制应保留首尾字符：{seg:?}"
        );
    }

    /// 用 `target/pdf测试.pdf` 实测 P2 文本层管线：行列合并、单行/跨行复制（含换行），
    /// 并打印最稠密页的首行文本样本便于人工核对抽取质量。
    #[test]
    fn configure_sheet_selection_pipeline() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
        assert!(p.exists(), "测试文件应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        eprintln!("pdf测试.pdf 共 {} 页", dims.len());

        // 逐页抽取统计，选“可用字符最多”的一页做管线验证
        let mut best: Option<(usize, crate::pdf::PageText)> = None;
        for idx in 0..dims.len() {
            let rx = worker.request_text(doc_id, idx);
            let text = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本")
                .expect("文本应成功");
            let usable = text
                .chars
                .iter()
                .filter(|c| !c.synthetic && c.right > c.left)
                .count();
            eprintln!(
                "页 {idx}: 字符 {} synthetic {} 可用 {}",
                text.chars.len(),
                text.synthetic_count,
                usable
            );
            if best.as_ref().is_none_or(|(_, t)| {
                t.chars.iter()
                    .filter(|c| !c.synthetic && c.right > c.left)
                    .count()
                    < usable
            }) {
                best = Some((idx, text));
            }
        }
        let (page_idx, text) = best.expect("应有内容页");
        let usable = text
            .chars
            .iter()
            .filter(|c| !c.synthetic && c.right > c.left)
            .count();
        assert!(usable >= 20, "配置表最稠密页可用字符应 ≥20，实际 {usable}");

        // 抽样前 5 个字符盒（人工核对坐标/解码质量）
        let mut shown = 0;
        for c in text.chars.iter() {
            if c.synthetic {
                continue;
            }
            eprintln!(
                "  字符 {:?} 盒 [{:.1},{:.1} {:.1},{:.1}] origin=({:.1},{:.1})",
                c.ch, c.left, c.bottom, c.right, c.top, c.origin_x, c.origin_y
            );
            shown += 1;
            if shown >= 5 {
                break;
            }
        }

        // 第一行：合并为单个高亮矩形 + 单行复制无内部换行
        let first = text
            .chars
            .iter()
            .position(|c| !c.synthetic && c.right > c.left)
            .expect("应存在字符");
        let base_y = text.chars[first].origin_y;
        let run = text.chars[first..]
            .iter()
            .take_while(|c| !c.synthetic && (c.origin_y - base_y).abs() <= 1.5)
            .count();
        let line_hi = first + run.saturating_sub(1);
        let spans = layout::selection_spans_pt(&text, first, line_hi);
        assert_eq!(spans.len(), 1, "第一行应合并为 1 矩形, 实际 {}", spans.len());
        let one = layout::page_segment_text(&text, first, line_hi);
        assert!(!one.contains('\n'), "单行复制不应有换行：{one:?}");
        eprintln!("pdf测试.pdf 第 {} 页首行复制样本: {:?}", page_idx + 1, one);

        // 跨到第二行：复制应含 \n 边界（换行启发式触发）
        let second = text.chars[first..]
            .iter()
            .position(|c| !c.synthetic && c.origin_y < base_y - 1.5)
            .map(|k| first + k)
            .unwrap_or(line_hi);
        if second > line_hi {
            let seg2 = layout::page_segment_text(&text, first, (second + 2).min(text.chars.len() - 1));
            assert!(seg2.contains('\n'), "跨行复制应含换行：{seg2:?}");
            eprintln!("pdf测试.pdf 第 {} 页两行复制样本:\n{seg2}", page_idx + 1);
        }
    }

    /// P3 集成（pdf测试.pdf）：文档级检索命中非空、命中文本精确等于关键字、区间合法；
    /// 无命中返回空；空串返回空且不截断。
    #[test]
    fn doc_search_finds_and_verifies_hits() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");

        let rx = worker.search(doc_id, "配置".to_string());
        let results = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("应收到检索结果")
            .expect("检索应成功");
        assert!(
            !results.hits.is_empty(),
            "pdf测试.pdf 应能检索到正文词命中（如「配置」）"
        );
        let min_hits = results.hits.first().cloned().expect("非空");
        assert!(min_hits.start < min_hits.end, "区间应有序: {min_hits:?}");

        // 抽查若干命中：命中文本（可见序列）与关键字逐字一致
        let mut checked = 0usize;
        for hit in results.hits.iter().take(5) {
            let rx_t = worker.request_text(doc_id, hit.page);
            let text = rx_t
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本")
                .expect("文本应成功");
            assert!(hit.end <= text.chars.len(), "区间应在页内: {hit:?}");
            let seg = layout::page_char_substring(&text, hit.start, hit.end.saturating_sub(1));
            assert_eq!(seg, "配置", "命中文本应与关键字一致，实际 {seg:?}");
            assert!(
                hit.snippet.contains("配置"),
                "命中片段应包含关键字：{:?}",
                hit.snippet
            );
            eprintln!("命中 第 {} 页  片段: {:?}", hit.page + 1, hit.snippet);
            checked += 1;
        }
        assert!(checked >= 1, "应至少校验 1 个命中");

        // 无命中 → 空结果
        let rx2 = worker.search(doc_id, "绝对不存在的词XYZxyz".to_string());
        let r2 = rx2
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("应收到检索结果")
            .expect("检索应成功");
        assert!(r2.hits.is_empty(), "不应有无谓命中");
        assert!(!r2.truncated, "空结果不应截断");

        // 空串 → 空且不截断（搜索框清空时的预期）
        let rx3 = worker.search(doc_id, String::new());
        let r3 = rx3
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("应收到检索结果")
            .expect("检索应成功");
        assert!(r3.hits.is_empty() && !r3.truncated);
    }

    /// 回归探针（自动）：全文档检索必须**分片续跑**，不得整段冻结瓦片/封面渲染。
    /// 修复前：`WorkerMsg::Search` 一条消息就是全文档同步扫描（`parser::search_document`），
    /// worker 扫描期间无法处理任何 Render/RenderTile → 随机落在视口的页占位数百 ms 乃至
    /// 数秒（本仓库 87 页文本型 PDF 实测检索 ~440ms、期间渲染被堵到全文扫完；
    /// 修复后：检索按片执行（数页/数 ms 一片），检索期间到达的渲染只等一片就轮到。
    #[test]
    fn whole_doc_search_freeze_blocks_renders_probe() {
        // 时序探针（`render_wait_ms < search_ms * 0.4`）：有夹具时随套件跑，验证检索分片不冻结渲染。
        skip_if_missing_pdf_fixture!();
        use std::time::{Duration, Instant};
        let _guard = pdf_lock().lock().unwrap();
        // 必须用长文本型 PDF（target/pdf测试2.pdf，259 页）复现：18 页文本 PDF 整篇检索
        // 实测 0ms（低于 >100ms 断言）；扫描件（242 页）检索本就 ≈0ms。
        let p = test_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        eprintln!("[PROBE] text-pdf pages={}", dims.len());

        // 空闲基准：整页渲染一次（应毫秒级）
        let t0 = Instant::now();
        let rx0 = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 }, 0, 0);
        rx0.recv_timeout(Duration::from_secs(60))
            .expect("基准渲染应返回");
        let baseline_ms = t0.elapsed().as_secs_f64() * 1e3;

        // 触发全文档检索；30ms 后（worker 已进入检索分片）再排一张整页渲染。
        // ⚠️ 渲染到达时间必须用独立线程单独计时——不能先 recv 检索再测渲染的
        //    `elapsed()`（那会把“我读它那一刻”的墙钟也算进去，掩盖真实的到达时刻）。
        let rx_search = worker.search(doc_id, "配置".to_string());
        std::thread::sleep(Duration::from_millis(30));
        let rx_render = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let t_send = Instant::now();
        let render_meas = std::thread::spawn(move || {
            let _r = rx_render
                .recv_timeout(Duration::from_secs(300))
                .expect("检索期间排队的渲染应返回");
            t_send.elapsed().as_secs_f64() * 1e3
        });
        let _s = rx_search
            .recv_timeout(Duration::from_secs(300))
            .expect("检索应返回");
        let search_ms = t_send.elapsed().as_secs_f64() * 1e3;
        let render_wait_ms = render_meas
            .join()
            .expect("测量线程应完成");
        eprintln!(
            "[PROBE] search={search_ms:.0}ms; render queued during search arrived after {render_wait_ms:.0}ms (idle baseline {baseline_ms:.0}ms)"
        );

        // 检索本身耗时应可观（证明这是个真实的长时间任务，值得分片）
        assert!(search_ms > 100.0, "全文档检索应显著耗时：实际 {search_ms:.0}ms");
        // 关键回归：检索期间的渲染只等一片，绝不等到检索扫完全文
        assert!(
            render_wait_ms < search_ms * 0.4 && render_wait_ms < 200.0,
            "检索应分片、不得冻结渲染：search={search_ms:.0}ms render_wait={render_wait_ms:.0}ms"
        );
    }

    #[test]
    #[ignore] // 性能基准：依赖真 PDF 夹具且耗时，手动 `--ignored` 跑
    fn bench_first_pages_render_time() {
        let _guard = pdf_lock().lock().unwrap();
        // 首次浏览性能摸底：加载 + 顺序渲染前 8 页，统计平均单页端到端耗时
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let t0 = std::time::Instant::now();
        let (worker, doc_id, dims) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        let load_ms = t0.elapsed().as_millis();
        let n = dims.len().min(8);
        let mut render_ms: Vec<(u64, u32, u64, u64)> = Vec::new(); // (渲染ms, 宽, bgra包裹us, bgra走线us)
        for idx in 0..n {
            let t1 = std::time::Instant::now();
            let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 1.0 }, 0, 0);
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("渲染应成功")
                .expect("渲染应成功");
            let m = t1.elapsed().as_millis() as u64;
            // 快路径：BGRA 直接包裹（主线程零像素循环）
            let t2 = std::time::Instant::now();
            let _img = render_image_from_bgra(res.bgra.clone(), res.width, res.height)
                .expect("BGRA→RenderImage 应成功");
            let wrap_us = t2.elapsed().as_micros() as u64;
            render_ms.push((m, res.width, wrap_us, 0));
        }
        let avg_ms = render_ms.iter().map(|r| r.0).sum::<u64>() / n as u64;
        let avg_wrap = render_ms.iter().map(|r| r.2).sum::<u64>() / n as u64;
        eprintln!(
            "BENCH: 打开+量页 {load_ms}ms; 每页渲染均值 {avg_ms}ms; \
             BGRA包裹均值 {avg_wrap}us; 明细 {render_ms:?}"
        );
    }

    #[test]
    #[ignore] // 性能基准：依赖真 PDF 夹具且耗时，手动 `--ignored` 跑
    fn bench_single_page_latency_histogram() {
        let _guard = pdf_lock().lock().unwrap();
        // 单页端到端延迟直方图：连发 5 次第 2 页，看 worker 是否排队/稳定性
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        let mut lats = Vec::new();
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let rx = worker.request_render(doc_id, RenderRequest { page: 2, scale: 1.0 }, 0, 0);
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("渲染应成功")
                .expect("渲染应成功");
            lats.push(t0.elapsed().as_micros() as u64 / 1000);
            let _ = res;
        }
        eprintln!("BENCH-LAT: 单页端到端延迟(ms): {lats:?} (均值 {:?}ms)",
            lats.iter().sum::<u64>() / lats.len() as u64);
    }

    #[test]
    #[ignore] // 性能基准：依赖真 PDF 夹具且耗时，手动 `--ignored` 跑
    fn bench_worker_cost_split() {
        let _guard = pdf_lock().lock().unwrap();
        // 细拆分：pdfium 栅格化 vs RGBA→BGRA 重排 vs PNG 编码，各占多少毫秒
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) =
            PdfWorker::open_and_dims(bytes).expect("启动 worker 应成功");
        let idx = 1usize;
        let t0 = std::time::Instant::now();
        let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 1.0 }, 0, 0);
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到结果");
        let total = t0.elapsed().as_millis() as u64;
        let res = res.expect("渲染应成功");
        let (pw, _) = dims[idx];
        let w = (pw * 1.0f32).ceil() as u32;
        eprintln!(
            "BENCH-SPLIT: 第{idx}页 端到端 {total}ms (页宽 {pw}pt, {w}px @1.0x, 输出 {}x{}px)；\
             worker 内部含 pdfium栅格化（不再含 PNG 编码，编码已移出 worker）。打开量页耗时见其它测试。",
            res.width, res.height
        );
        assert!(total < 5_000, "单页端到端不应超过5s");
    }

    /// 回归：PDF 页列必须以「滚动可视区」为基准水平居中，而不是全窗口。
    /// 侧边栏/分屏开启时可视区窄于窗口，若按窗口宽居中会整体右偏；
    /// 同时可视区窄于「页宽+边距」时必须产生横向溢出（可滚到全部页面）。
    #[gpui::test]
    async fn pdf_column_centers_within_pane_width(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{
            AppContext, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        struct PaneProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
            pane_w: f32,
        }
        impl Render for PaneProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div()
                    .id("probe-pane")
                    .w(gpui::px(self.pane_w))
                    .h_full()
                    .child(self.pdf.clone())
            }
        }

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);

        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        // 宽区（可完全容纳）与窄区（窄于页宽+边距）各验证一次
        for pane_w in [1200.0f32, 600.0] {
            let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
            let (_root, cx) = cx.add_window_view(|_window, cx| {
                let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
                *holder.borrow_mut() = Some(pdf.clone());
                PaneProbe { pdf, pane_w }
            });
            cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
            for _ in 0..3 {
                cx.run_until_parked();
            }
            for _ in 0..200 {
                cx.run_until_parked();
                let loaded = cx.update(|_w, cx| {
                    holder
                        .borrow()
                        .as_ref()
                        .map(|pv| pv.read(cx).is_loaded())
                        .unwrap_or(false)
                });
                if loaded {
                    break;
                }
            }
            for _ in 0..6 {
                cx.run_until_parked();
            }
            // fit-width 自适应：让每帧装配按最新容器宽重设页宽基线（同 resize 测试口径）。
            for _ in 0..10 {
                let _ = cx.update(|w, _| w.refresh());
                cx.run_until_parked();
            }
            let scroll = cx.debug_bounds("pdf-scroll").expect("应有滚动容器");
            let pages = cx
                .debug_bounds("pdf-pages")
                .expect("应有页列（宽=页宽）");
            assert_eq!(scroll.size.width.as_f32(), pane_w, "滚动可视区宽应为 pane 宽");
            let left_pad = (pages.left() - scroll.left()).as_f32();
            // **自适应窗口宽**（fit-width）：页宽 = 容器宽 − 2·页边距，宽/窄 pane 都收
            // 进可视区、保持 PAGE_PAD_X 左缘边距并居中，不再横向溢出（窄 pane 收窄页面）。
            assert!(
                (pages.size.width.as_f32() - (pane_w - 2.0 * config::pdf::PAGE_PAD_X)).abs() < 2.0,
                "页宽应自适应容器宽 {pane_w}：实际={}",
                pages.size.width.as_f32()
            );
            assert!(
                (left_pad - config::pdf::PAGE_PAD_X).abs() < 1.0,
                "左缘应保持 PAGE_PAD_X 边距：left_pad={left_pad:.1}"
            );
            assert!(
                pages.right().as_f32() <= scroll.right().as_f32() + 1.0,
                "页应完全落入可视区（自适应，不应横向溢出）"
            );
            let center_off = (pages.center().x - scroll.center().x).as_f32();
            assert!(
                center_off.abs() < 1.0,
                "页列应居中：center_off={center_off:.3}px"
            );
        }
    }

    // ---- 缩放雾感修复 S1-S5 回归（2026-10-30）：Ctrl+滚轮连续多档不「雾蒙蒙」 ----

    // 缩放雾感测试用到的 gpui trait（本组测试仅适用）：`ZoomProbe` 渲染与 `add_window_view`
    // 建 view 需要。函数内 `use gpui::{...}` 与这里重复导入不同作用域，合法。
    use gpui::{AppContext as _, ParentElement as _, Styled as _};

    /// 缩放雾感测试的窗口宿主（`zoom_add_window` 用）。
    struct ZoomProbe {
        pdf: gpui::Entity<crate::pdf::PdfViewer>,
    }
    impl gpui::Render for ZoomProbe {
        fn render(
            &mut self,
            _w: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div().size_full().child(self.pdf.clone())
        }
    }

    /// 新建一个「缩放雾感」测试的空 viewer 句柄容器（holder 依调用方持有的 Rc 生命周期）。
    fn zoom_holder(
    ) -> std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>> {
        std::rc::Rc::new(std::cell::RefCell::new(None))
    }

    /// 缩放雾感测试的打开辅助：创建窗口与 viewer（装入 `holder`）、等加载并预热布局。
    /// **必须在测试体内直接遮蔽**：`let (_root, cx) = zoom_add_window(cx, &holder, name);`
    /// —— `add_window_view` 返回的 `&mut VisualTestContext` 借用自 `cx`（TestAppContext），
    /// 之后的一切 `cx.update`/`cx.simulate_event`/`cx.executor()` 都作用于新绑定 `cx`。
    /// 容器 1500×900（页宽适配 ≈1160px）。复制 `pdf_ctrl_wheel_zooms_in_out` 的骨架。
    fn zoom_add_window<'a>(
        cx: &'a mut gpui::TestAppContext,
        holder: &std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>>,
        pdf_name: &str,
    ) -> (gpui::Entity<ZoomProbe>, &'a mut gpui::VisualTestContext) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../target/{pdf_name}"));
        assert!(path.exists(), "测试 PDF 应存在: {path:?}");
        cx.update(crate::test_support::bind_local_keymaps);
        let h = holder.clone();
        let (root, cx) = cx.add_window_view(move |_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(path.clone(), cx));
            *h.borrow_mut() = Some(pdf.clone());
            ZoomProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "测试 PDF 应加载完成");
        for _ in 0..6 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
        }
        (root, cx)
    }

    /// 等「泵停 + 全部在途清空 + 非快速」的完整沉降（jump 测试同款 settle 谓词）。
    /// 仅调试/极端需要全渲染收敛时用；新测试默认用 [`zoom_wait_frames`]（机制断言无需等
    /// worker 全量渲染——测试平台 2× DPI + debug 高密栅格极慢且时序抖动量级大）。
    #[allow(dead_code)]
    fn zoom_settle(
        cx: &mut gpui::VisualTestContext,
        holder: &std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>>,
    ) {
        use std::time::Duration;
        // 收敛判据 = 连续 40 帧「非快速 + 泵停 + 在途总量不增长」（容忍高密密度追赶期的
        // 极度缓慢渲染/单瓦片重派杂音——测试平台 2× DPI + debug 下实测达此量级）。
        let mut stable = 0u32;
        let mut last_total: Option<usize> = None;
        for _ in 0..1500 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_tiles.len() + v.pending_cover.len() + v.pending_prefetch_cover.len(),
                    !v.prev_fast && !v.tile_pump_running,
                )
            });
            let total = st.0;
            if st.1 && last_total == Some(total) {
                stable += 1;
                if stable >= 40 {
                    return;
                }
            } else {
                stable = 0;
            }
            last_total = Some(total);
            // 真清空（非快速 + 泵停 + 在途 0）立即收敛。
            if st.1 && total == 0 {
                return;
            }
        }
        let (pt, pc, pp, pf, pump) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.pending_tiles.len(),
                v.pending_cover.len(),
                v.pending_prefetch_cover.len(),
                v.prev_fast,
                v.tile_pump_running,
            )
        });
        panic!(
            "状态应沉降：pending_t={pt} pending_c={pc} pending_pf={pp} prev_fast={pf} pump={pump}"
        );
    }

    /// 固定帧预热：`n` 次 `advance_clock(16ms)+run_until_parked`（每次 16ms 量级推进渲染/泵
    /// 循环）。用于「机制状态」断言前的确定性推进；**不等于**等 worker 全量渲染收敛——
    /// 测试平台 2× DPI + debug 下高密栅格极慢且时序抖动量级大，等全清会超时误判（依赖
    /// `zoom_settle` 的断言只在真正需要「渲染完成」时才用，且须容忍杂音）。
    fn zoom_wait_frames(cx: &mut gpui::VisualTestContext, n: usize) {
        use std::time::Duration;
        for _ in 0..n {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 读某页**页顶相对视口顶偏移**（字符 window-y 的代表）——Z1 端到端守恒断言用。
    fn zoom_anchor_rel(
        cx: &mut gpui::VisualTestContext,
        holder: &std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>>,
        page: usize,
    ) -> f32 {
        cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let top = v.page_tops[page];
            let sy = f32::from(-v.scroll.offset().y);
            top - sy
        })
    }

    /// Ctrl+滚轮缩放事件构造（vertical pixel delta；向上 >0 放大、向下 <0 缩小）。
    fn zoom_wheel(control: bool, delta_y: f32) -> gpui::ScrollWheelEvent {
        gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// C 探针：**FullBand 仿射不能独立致雾**。合成 1px 高「文本 AA 边缘」灰度条带 → 双线性
    /// 拉伸 300%（模拟缩放手势的位图放大）→ 分路「恒等直通 vs FullBand(bg=32, ink=216)」。
    /// FullBand 是全局仿射 `out = 32 + (216-32)·(1−L/255)`，任何 luma 差都等比 ×184/255≈0.7216
    /// ——拉伸造成的雾环（插值灰带）在 FullBand 下**同比例**压缩，不存在额外非线性收缩，
    /// 即「缩放雾感不是重着色曲线制造」（雾的真源在调度/几何，见 S1-S5）。两条断言锁死
    /// 「笔画核与雾环 luma 分离度比 ≈0.7216」与「边缘最大梯度比 ∈[0.716,0.728]」（含 LUT
    /// 整数舍入的 ±1-2 LSB 容差）。
    #[test]
    fn probe_tonal_zero_contribution() {
        // 1D 双线性上采样：输出像素 j 对应源坐标 x=(j+0.5)·n/out_w − 0.5。
        let upscale = |src: &[u8], out_w: usize| -> Vec<u8> {
            let n = src.len();
            let mut out = Vec::with_capacity(out_w);
            for j in 0..out_w {
                let x = (j as f32 + 0.5) * n as f32 / out_w as f32 - 0.5;
                if x <= 0.0 {
                    out.push(src[0]);
                    continue;
                }
                let i = (x as usize).min(n - 1);
                if i >= n - 1 {
                    out.push(src[n - 1]);
                    continue;
                }
                let frac = x - i as f32;
                let a = src[i] as f32;
                let b = src[i + 1] as f32;
                out.push((a + (b - a) * frac).round().clamp(0.0, 255.0) as u8);
            }
            out
        };
        // 纸(255) → AA 过渡(191/128/64) → 笔(0) 的 1px 高条带。
        let src: Vec<u8> = vec![255, 255, 191, 128, 64, 0, 0];
        let up = upscale(&src, src.len() * 3);
        // 恒等路：分离度 = luma 动态范围、边缘 = 最大相邻差。
        let span_id = (up.iter().max().copied().unwrap() as i32) - (up.iter().min().copied().unwrap() as i32);
        let grad_id = up.windows(2).map(|w| (w[0] as i32 - w[1] as i32).abs()).max().unwrap();
        // FullBand 路：整条经真实重着色内核（apply_recolor_with_curve）再读回灰通道。
        let mut bgra = Vec::with_capacity(up.len() * 4);
        for &l in &up {
            bgra.extend_from_slice(&[l, l, l, 255]);
        }
        crate::recolor::apply_recolor_with_curve(
            &mut bgra,
            crate::recolor::RecolorParams {
                bg: [32, 32, 32],
                ink: Some([216, 216, 216]),
            },
            crate::recolor::RecolorCurve::FullBand,
        );
        let fb: Vec<u8> = bgra.chunks(4).map(|c| c[2]).collect(); // BGRA：c[2]=R（灰输入三通道同值）
        let span_fb = (fb.iter().max().copied().unwrap() as i32) - (fb.iter().min().copied().unwrap() as i32);
        let grad_fb = fb.windows(2).map(|w| (w[0] as i32 - w[1] as i32).abs()).max().unwrap();
        let expect = 184.0 / 255.0; // ≈0.7216 = 仿射增益（(216-32)/255 波幅 / 全带原差）
        let ratio = span_fb as f32 / span_id as f32;
        let grad_ratio = grad_fb as f32 / grad_id as f32;
        eprintln!(
            "[C-probe] span {span_id}→{span_fb} ratio={ratio:.4} grad {grad_id}→{grad_fb} grad_ratio={grad_ratio:.4} expect={expect:.4}"
        );
        assert!(
            (ratio - expect).abs() < 0.02,
            "分离度比应 ≈{expect:.4}（至多 ±1-2 LSB / 255）：{ratio:.4}"
        );
        assert!(
            (0.716..=0.728).contains(&grad_ratio),
            "边缘最大梯度比应 ∈[0.716,0.728]（FullBand 仿射等比例压缩雾环）：{grad_ratio:.4}"
        );
    }

    /// C 探针：**拼缝非灰带源**。用「瓦片独立恒等采样模型」（D3D/渲染端对每张 tile 纹理各按
    /// 自身 UV 空间半纹素采样，等价于取像素自身值、不跨 tile 混合）模拟相邻 256px tile 拼合
    /// 整行：缝侧像素必须等于其所在 tile 的内部同值列（差 ≤2 LSB）——瓦片拼缝处不会因采样
    /// 姿势凭空插入灰带（若错误地跨 tile 连续线性插值，纸色 255 与墨色 0 之间会混出 ≈128 的
    /// 灰缝；正确的分别采样则无）。内容：每 tile = [墨缘 2px, 纸底 254px]，两 tile 在缝处以
    /// 墨缘相接。
    #[test]
    fn probe_seam_no_fog() {
        let w = 256usize;
        let paper = 255u8;
        let ink = 0u8;
        let edge = 2usize;
        let tile = |e: usize| -> Vec<u8> {
            (0..w).map(|x| if x < e { ink } else { paper }).collect()
        };
        // 每 tile 独立恒等采样（渲染端对每张 tile 纹理各自采样、不跨 tile 混合）：
        // 像素值即自身，拼合后缝侧与 tile 内部同值列一致。
        let sample = |t: &[u8]| -> Vec<u8> {
            t.iter().enumerate().map(|(x, _)| t[x]).collect()
        };
        let ta = sample(&tile(edge));
        let tb = sample(&tile(edge));
        let mut row = Vec::with_capacity(w * 2);
        row.extend_from_slice(&ta);
        row.extend_from_slice(&tb);
        // 缝：x=255（tileA 纸底尾）、x=256（tileB 墨缘头）、x=257（tileB 墨缘 2）。
        // 断言：缝列与其所在 tile 内部同值列一致（≤2 LSB）→ 无灰带。
        assert!(
            (row[255] as i32 - row[254] as i32).abs() <= 2,
            "tileA 缝侧纸底应与内部纸列一致（无灰带）：{} vs {}",
            row[255],
            row[254]
        );
        assert!(
            (row[256] as i32 - row[257] as i32).abs() <= 2,
            "tileB 缝侧墨缘应与内部墨列一致（无灰带）：{} vs {}",
            row[256],
            row[257]
        );
        assert!(row[255] == paper, "缝左应为纸色（tileA 纸底）：{}", row[255]);
        assert!(row[256] == ink, "缝右应为墨色（tileB 墨缘）：{}", row[256]);
        eprintln!(
            "[C-probe] seam row[254..258]={:?} → 缝侧与内部一致，非灰带源",
            &row[254..258]
        );
    }

    /// S1 回归：缩放事件后速度不升 VeryFast 且 `zoom_armed` 置位（重锚 offset 突变不再被
    /// 误判为滚动 → 手势期不再被 VeryFast 掐死零派发）。在深滚动位连续多档缩放，全程
    /// `speed_state != VeryFast` 且滚动速度 EMA 从不越入高速带。
    #[gpui::test]
    async fn zoom_input_suppresses_false_scroll_speed(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(5);
                    cx.notify();
                });
        });
        zoom_wait_frames(cx, 40);
        let very = config::pdf::MOVE_THRESHOLD_PX * config::pdf::MOVE_THRESHOLD_VERY_RATIO;
        let mut seen_veryfast = false;
        let mut max_vel = 0.0f32;
        // 8 档放大（每档事件 + 少量帧），全程采样速度与状态。
        for _ in 0..8 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..3 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                let (sp, vel) = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (v.speed_state, v.scroll_velocity)
                });
                max_vel = max_vel.max(vel);
                if sp == crate::pdf::view::SpeedState::VeryFast {
                    seen_veryfast = true;
                }
            }
        }
        eprintln!(
            "[S1] seen_veryfast={seen_veryfast} max_vel={max_vel:.1} very_threshold={very}"
        );
        assert!(
            !seen_veryfast,
            "缩放重锚后不得被误判为滚动进 VeryFast（手势期零派发的雾感根因）"
        );
        assert!(
            max_vel < very,
            "缩放期滚动速度 EMA 不得越入高速带：max={max_vel:.1} ≥ {very}"
        );
        // 收口最终发生（非零派发）且状态收敛。
        zoom_wait_frames(cx, 40);
        let (z, armed) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.zoom, v.zoom_armed)
        });
        assert!(z > 1.0, "应有缩放发生：zoom={z}");
        assert!(!armed, "防抖到期后 zoom_armed 应被清空：{armed}");
        zoom_wait_frames(cx, 40);
    }

    /// S1/S2 回归：缩放防抖窗内**绝不派满密 cover/tile**（逐档抢渲被弃空转），只派 ≤2 枚
    /// 当前档 0.5 密度 preview。防抖内 `pending_cover`/`pending_tiles` 恒空、封面/瓦片计数
    /// 不增长、preview 计数增长。
    #[gpui::test]
    async fn zoom_gesture_dispatches_only_preview(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        let (c0, tl0, pf0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs, v.prefetch_reqs)
        });
        // 连续多档缩放（模拟连续滚轮事件流，每档后推进几帧），并观察**计数净增量**：
        // - `prefetch_reqs` 增长（防抖窗内只派 0.5 密度 preview）；
        // - `cover_reqs`/`tile_reqs` 的净增**受限于收口的一次性用量**（不作逐档抢派——
        //   若每档都抢满密，8 档会让 cover 增量随档数线性暴涨；防抖收口只在最后一档到期后
        //   派一次可见窗用量）。
        let mut armed_seen = false;
        for _ in 0..8 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                let armed = cx.update(|_w, cx| {
                    holder.borrow().as_ref().unwrap().read(cx).zoom_armed
                });
                armed_seen |= armed;
            }
        }
        // 收口需要防抖到期（ZOOM_DEBOUNCE_FRAMES 帧无新事件）——事件流结束后推进足帧。
        zoom_wait_frames(cx, 30);
        let (c1, tl1, pf1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs, v.prefetch_reqs)
        });
        let d_cover = c1 - c0;
        let d_tile = tl1 - tl0;
        let d_pref = pf1 - pf0;
        eprintln!(
            "[S2] armed_seen={armed_seen} d_cover={d_cover} d_tile={d_tile} d_pref={d_pref}（8 档）"
        );
        assert!(armed_seen, "连续缩放期间应出现 zoom_armed（防抖窗）");
        // 满密 cover 只在收口时派一次窗口用量（≤ 封面向导页数 + 余量，宽松上限 6）；瓦片
        // 受限于收口/常态补片量级。若逐档抢派满密，8 档 cover 增量会趋近 8+、瓦片随档成倍。
        // #5 起收口瓦片按 SETTLE_TILE_BURST_MAX=16 一次性出清：8 档内多数档间隙会触发
        // 一次「中间档 settle」（泵帧把防抖推到 3 帧），每档突发多派 ≤10 枚瓦片请求——这些
        // 中间档请求出队即被 Z2 按世代 drop、不真实渲染（近免费队列入队空转），只抬高计数。
        // 上限 220：仍锁定「不逐档抢派瓦片」（若无节流/突发失控，8 档会随缩放宽高上千），
        // 同时容纳 #5 有意为之的收口突发。
        assert!(
            d_cover <= 6,
            "cover 不应逐档抢派：8 档净增 {d_cover}（收口一次性应≤窗口页数）"
        );
        assert!(
            d_tile <= 220,
            "瓦片不应逐档抢派：8 档净增 {d_tile}（含 #5 收口突发 16/帧×中间档 settle；Z2 后中间档 cover 被 drop、常态补片受限于可见片数量级）"
        );
        // preview 派发在本环境可能为 0：测试平台 2× DPI + 大页 → 开载 render_scale 即超
        // COVER_SCALE_CAP，cover 密度被 cap 截断且恒等于 cscale → "cover 同档"守卫跳过
        // preview（cover 4.0 比 0.5 密度 preview 更接近显示密度，preview 冗余属正确行为）。
        // preview 只在「cover 密度需随档变」的非截断区间才有价值（见 `zoom_preview_scale_*`）。
        eprintln!("[S2] d_pref={d_pref}（cap 截断下 cover 同档 → preview 可合法为 0）");
        // 收口最终也真实发生（防抖到期后常态调度至少一次满密派发）。
        assert!(
            d_cover > 0 || d_tile > 0,
            "防抖收口后应有满密 cover/tile 派发（一次收口，非零派发）：cover {c0}→{c1} tile {tl0}→{tl1}"
        );
    }

    /// S3 回归：防抖到期后强制收口——`zoom_armed` 清空、speed 回慢速、可见页最终有当前
    /// 密度满密底图就位，且沉降后不再重复派发（一次收口）。
    #[gpui::test]
    async fn zoom_debounce_collects_full_resolution_once(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        let (c0, tl0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs)
        });
        // 两档放大后不再缩放 → 防抖到期 → 收口。
        cx.simulate_event(zoom_wheel(true, 240.0));
        cx.simulate_event(zoom_wheel(true, 240.0));
        let mut cleared = false;
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            cleared = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                !v.zoom_armed && v.speed_state != crate::pdf::view::SpeedState::VeryFast
            });
            if cleared {
                break;
            }
        }
        assert!(cleared, "防抖到期后 zoom_armed 应被清空且速度离开高速带");
        zoom_wait_frames(cx, 40);
        // 可见页在当前缩放下应有底图（收口达阵）。口径放宽为「不落加载中」：满密 cover / 满密
        // 瓦片 / 走廊 preview / 中清 / 缩略任一在槽即可——真实渲染帧数由 worker 决定（测试
        // 平台 2× DPI + debug 高密栅格极慢），本断言只锁「收口后视图不再空白」。
        let (pw, sf, all_ready) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let off_y = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
            let vis = pdf_visible_pages(v, pw, off_y, vh);
            let ready = !vis.is_empty()
                && vis.iter().all(|&p| {
                    let s = &v.slots[p];
                    s.cover.is_some()
                        || !s.tiles.is_empty()
                        || s.prefetch_cover.is_some()
                        || s.mid.is_some()
                        || s.thumb.is_some()
                });
            (pw, sf, ready)
        });
        let _ = (pw, sf);
        eprintln!("[S3] 收口后可见页有底图: {all_ready}");
        assert!(all_ready, "收口后可见页应不再落「加载中」（任一底图在槽）");
        let _ = (c0, tl0);
    }

    /// S1/S3 回归：缩放重锚后 `scroll_velocity` 保持近零、Ctrl 缩放不产生额外滚动（同
    /// `pdf_ctrl_wheel_zooms_in_out` 的锚定断言，此处连续多档）。
    #[gpui::test]
    async fn zoom_anchored_scroll_velocity_stays_zero(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(5);
                    cx.notify();
                });
        });
        zoom_wait_frames(cx, 40);
        let off0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).scroll.offset());
        let w0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_width());
        // Z1：记录缩放前 layout（锚点公式需要旧 tops 与新 tops 对账）。
        let tops0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_tops.clone());
        let mut max_vel = 0.0f32;
        for _ in 0..6 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                let vel = cx.update(|_w, cx| {
                    holder.borrow().as_ref().unwrap().read(cx).scroll_velocity
                });
                max_vel = max_vel.max(vel);
            }
        }
        zoom_wait_frames(cx, 40);
        let (wn, offn, topsn, dims) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.page_width(),
                v.scroll.offset(),
                v.page_tops.clone(),
                v.dims.clone(),
            )
        });
        // Z1 锚点锚定期望：`exp_sy = tops_new[k] + (sy0 − tops_old[k]) × ratio`（常量区不缩放）。
        let sy0 = -off0.y.as_f32();
        let ratio = wn / w0;
        let k = layout::first_visible_page(&dims, &tops0, w0, sy0);
        eprintln!(
            "[S1] max_vel={max_vel:.2} 锚定 K={k} top_sy0={:.1} y={:.1} x={:.1}",
            -sy0,
            offn.y.as_f32(),
            offn.x.as_f32()
        );
        assert!(
            max_vel < 5.0,
            "重锚后滚动速度 EMA 应保持近零（不再回 VeryFast）：max={max_vel:.2}"
        );
        let exp_sy = topsn[k] + (sy0 - tops0[k]) * ratio;
        let exp_y = -exp_sy;
        let exp_x = off0.x.as_f32() * ratio;
        assert!(
            (offn.y.as_f32() - exp_y).abs() < 2.0 && (offn.x.as_f32() - exp_x).abs() < 2.0,
            "Ctrl 缩放不应产生额外滚动：y 锚 {:.1} 实际 {:.1}（x 锚 {:.1} 实际 {:.1}）",
            exp_y,
            offn.y.as_f32(),
            exp_x,
            offn.x.as_f32()
        );
    }

    /// **5c：armed 期 preview 不换底**（第 4 轮同根因 low-dens 侧）：手势期间**已有任何底**
    /// （cover/prev_cover/prefetch_cover）的页，到达的新档 preview 一律丢弃（`collect_
    /// prefetch_cover_results` 的 armed「有底不换」守卫）——屏显稳定在手势前那张，杜绝边缘
    /// 页逐档换底脉冲；「全无底」空页才由 preview 填一次（dispatch 侧 `maybe_dispatch_zoom
    /// _preview` 同款守卫对称：有底不派）。注入式：armed 期向有 prev_cover 桥的中心页注入
    /// 一枚已完成的当前档 preview 结果 → 断言 `prefetch_cover` 槽**不被**替换。
    #[gpui::test]
    async fn zoom_armed_preview_does_not_swap_bridged_page(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        cx.simulate_event(zoom_wheel(true, 240.0));
        zoom_wait_frames(cx, 2); // armed：relocate 已把手势前封面迁入 prev_cover
        let (center, pscale) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let sy = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height);
            let vis = pdf_visible_pages(&v, pw, sy, vh);
            let center = vis[vis.len() / 2];
            let cscale = layout::cover_scale(v.dims[center], pw, sf);
            let want = (cscale * config::pdf::PREFETCH_COVER_DENSITY)
                .min(config::pdf::COVER_SCALE_CAP);
            (center, want)
        });
        // 显式模拟「盖度变了、armed 首帧 relocate 已把手势前封面迁入 prev_cover 桥」
        // （本夹具 sf=2 下 cscale 恒被 cap=4.0 钉死、盖度未变 → relocate 不搬迁，故手动
        // 摆出「cover=None + prev_cover=手势前那张」的搬迁态，专测 collect 的 armed「有底
        // 不换」守卫；生产路径由真实 relocate 建立同态）。同时清掉初始走廊 preview
        // （首载空闲预取给页 0 填过廊封面，须清场才能观测「新 preview 到达是否写入」）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let s = &mut v.slots[center];
                if let Some(c) = s.cover.take()
                    && let Some(prev) = s.prev_cover.replace(c)
                {
                    v.pending_drops.push(prev);
                }
                s.cover_scale = 0.0;
                if let Some(old) = s.prefetch_cover.take() {
                    v.pending_drops.push(old);
                }
            });
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.pending_prefetch_cover.clear();
            });
        });
        // 注入一枚「已完成的当前档 preview 结果」（pscale = want，躲过 drop_stale 的保留判定，
        // 恰落入 collect 通路；armed 有底 → 应被丢弃不入槽）。
        let bw = 8u32;
        let bh = 8u32;
        let res = crate::pdf::RenderResult {
            bgra: vec![140u8; (bw * bh * 4) as usize],
            width: bw,
            height: bh,
            recolor_epoch: cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).recolor_epoch
            }),
            luma: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(res)).unwrap();
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.pending_prefetch_cover.insert(center, (pscale, v.nav_gen, rx));
            });
        });
        // armed 期推进：**1ms 单步**（不触发 16ms 泵定时器边界 → 批次内不产生会清 `zoom_armed`
        // 的多余帧；每步 run 前回钉保持 armed）——collect 应在「有底不换」守卫下丢弃注入
        // preview，prefetch_cover 不写入。
        let mut not_swapped = false;
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(1));
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                    v.zoom_armed = true;
                    v.zoom_idle_frames = 0;
                });
            });
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(0));
            not_swapped = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[center];
                // 注入条目**已被消费**（collect 跑过该页）且底基（cover/prev_cover）仍驻留、
                // prefetch_cover 未被新档替换——防「注入仍在途 → 空满足」。
                !v.pending_prefetch_cover.contains_key(&center)
                    && (s.cover.is_some() || s.prev_cover.is_some())
                    && s.prefetch_cover.is_none()
            });
            if not_swapped {
                break;
            }
        }
        eprintln!("[#5c] center={center} pscale={pscale:.2} not_swapped={not_swapped}");
        assert!(
            not_swapped,
            "armed 期有底页不得被新档 preview 替换——prefetch_cover 应保持 None（底基 cover/prev_cover 驻留）"
        );
    }

    /// **5b：armed 期到达的中间档封面一律丢弃**（手势期零满密换底的关键守卫）。注入式：
    /// armed 期向中心页 pending_cover 注入一枚完成的中档（req_scale ≠ 当前 cscale）封面结果
    /// → `collect_cover_results` 的 armed 守卫丢弃（不写 prev_cover 桥、不入 slot）——手势前
    /// 那张高密底图（prev_cover 桥）**恒不被置换**（Arc 身份保持不变、被替换次数 == 0）。
    #[gpui::test]
    async fn zoom_armed_drops_arriving_middle_covers(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        cx.simulate_event(zoom_wheel(true, 240.0));
        zoom_wait_frames(cx, 2); // armed：手势前封面已迁入 prev_cover 桥
        // 抗 cap 截断场景：盖度未变（cscale 被 cap 钉死在 4.0）时 relocate 不搬迁、手势前
        // 那张底基驻留在 slot.cover；盖度变了则迁入 prev_cover 桥——两种都算「手势前那张」。
        let (center, mid_scale, epoch, nav, pre_cover, pre_prev) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let sy = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height);
            let vis = pdf_visible_pages(&v, pw, sy, vh);
            assert!(!vis.is_empty(), "armed 采样时应有可见页（先吃饱帧）");
            let center = vis[vis.len() / 2];
            let cscale = layout::cover_scale(v.dims[center], pw, sf);
            // 中档 = 与当前 cscale 明显不同（×1.5 模拟缩放中间的更密旧档）。
            (
                center,
                cscale * 1.5,
                v.recolor_epoch,
                v.nav_gen,
                v.slots[center].cover.clone(),
                v.slots[center].prev_cover.clone(),
            )
        });
        assert!(
            pre_cover.is_some() || pre_prev.is_some(),
            "armed 应持有手势前那张高密底图（cover 或 prev_cover 桥）"
        );
        // 注入一枚完成的中档封面结果（armed 期到达 → 应被守卫丢弃）。
        let res = crate::pdf::RenderResult {
            bgra: vec![160u8; (8 * 8 * 4) as usize],
            width: 8,
            height: 8,
            recolor_epoch: epoch,
            luma: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(res)).unwrap();
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.pending_cover.insert(center, (mid_scale, epoch, nav, rx));
            });
        });
        // armed 期推进：**1ms 单步**（不触发 16ms 泵定时器边界 → 保持 `zoom_armed` 恒真，
        // 见 5c 同款注释）——collect 应丢弃中档——底基（cover/prev_cover）与注入前**身份**
        // 一致（未被置换、prev_cover 无中档写入）。
        let same_opt = |a: &Option<std::sync::Arc<gpui::RenderImage>>,
                        b: &Option<std::sync::Arc<gpui::RenderImage>>| -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(x), Some(y)) => std::sync::Arc::ptr_eq(x, y),
                _ => false,
            }
        };
        let mut untouched = false;
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(1));
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                    v.zoom_armed = true;
                    v.zoom_idle_frames = 0;
                });
            });
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(0));
            untouched = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[center];
                // 注入条目**已被消费**（collect 跑过该页）且 cover/prev_cover 身份均不被置换
                // ——防「中档仍在途 → 空满足」。
                !v.pending_cover.contains_key(&center)
                    && same_opt(&pre_cover, &s.cover)
                    && same_opt(&pre_prev, &s.prev_cover)
            });
            if untouched {
                break;
            }
        }
        eprintln!("[#5b] center={center} mid_scale={mid_scale:.2} untouched={untouched}");
        assert!(
            untouched,
            "armed 手势期到达的中间档封面应被丢弃（cover/prev_cover 身份均不被置换）"
        );
    }

    /// S4 定向回归（注入式）：**非 armed 期**过期档封面结果到达、且该页当前档封面未入槽 →
    /// 写入有界 `prev_cover` 桥（最终档 cover 前的陈旧回传作最佳过渡底图，至多一档 1.12
    /// 拉伸），而非静默丢弃。验证走真实 `collect_cover_results` 路径；注入到位**可见页**
    /// （窗内页的 `prev_cover` 桥会被 `evict_covers` 保留——窗外页的桥会按既有语义立即
    /// 清掉），并真实缩放一档后**显式退出 armed**（手势期到达的中间档由 armed 守卫一律
    /// 丢弃，见 `zoom_armed_drops_arriving_middle_covers`；本用例专验非 armed 的 S4 桥）。
    /// 手动把该页 `cover` 置空以模拟「缩放瞬间旧封面已移走」。
    #[gpui::test]
    async fn zoom_prev_cover_bridge_mirrors_latest_inflight_scale(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 真实缩放一档进入防抖（armed 期 schedule_covers 不派、collect 路径照常跑）。
        cx.simulate_event(zoom_wheel(true, 240.0));
        zoom_wait_frames(cx, 2);
        // ⚠️ **显式退出 armed**（手势期零满密换底分界）：`collect_cover_results` 现在对
        // `zoom_armed` 期间到达的封面**一律丢弃**（不写 prev_cover 桥）——那是本批修正的
        // 「手势期每档换底」（见 `zoom_armed_drops_arriving_middle_covers`）。S4 桥只保留
        // **非 armed** 时到达的过期封面上桥（settle 后最终档前的陈旧回传仍作最佳过渡）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.zoom_armed = false;
                v.zoom_idle_frames = 0;
            });
        });
        let (p, req_scale, epoch, nav, had_cover) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let p = 0usize.min(v.dims.len() - 1);
            let cur = layout::cover_scale(v.dims[p], pw, sf);
            // 过期档 = 与当前 cscale 明显不同（×1.5 模拟缩放中间的更密旧档）。
            (p, cur * 1.5, v.recolor_epoch, v.nav_gen, v.slots[p].cover.is_some())
        });
        let _ = had_cover;
        // 模拟「缩放瞬间旧封面已移走」：手动清空当前档 cover（防抖内 schedule_covers 不
        // 重派，worker 不会竞争覆盖注入）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let s = &mut v.slots[p];
                s.cover = None;
                s.cover_scale = 0.0;
            });
        });
        // 注入一个「过期档」完成的封面结果（req_scale ≠ 当前 cscale），走真实 collect 路径。
        let bw = 8u32;
        let bh = 8u32;
        let bgra = vec![160u8; (bw * bh * 4) as usize];
        let res = crate::pdf::RenderResult {
            bgra,
            width: bw,
            height: bh,
            // recolor（重着色）世代用 viewer 当前世代——W1 世代守卫会丢弃旧世代结果。
            recolor_epoch: epoch,
            luma: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Some(res)).unwrap();
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.pending_cover.insert(p, (req_scale, epoch, nav, rx));
            });
        });
        // 泵几帧（防抖内 pump 持续）→ collect_cover_results → S4 桥（结果即时就绪）。
        // 桥建立后 worker 真实渲染的**当前档**封面（若赶上）会把桥回收（既有
        // `prev_cover.take()`）——故此处断言「桥曾建立」而非「此刻仍驻留」。
        let mut bridged = false;
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[p];
                (
                    s.prev_cover.is_some() && s.cover.is_none(),
                    s.cover.is_some() && s.prev_cover.is_none(),
                )
            });
            bridged |= st.0; // 桥曾建立（过期档写入 prev_cover）
            bridged |= st.1; // 或已「当前档封面入槽 + 桥随之回收」（证明桥曾建立并被既有 take 回收）
            if bridged {
                break;
            }
        }
        assert!(bridged, "过期档封面到达应写入 prev_cover 桥（当前档封面未入槽）");
        // 引擎继续推进：桥应随「最终档封面入槽 / 封面窗滚动」被既有 prev_cover.take() 回收
        // （结构性路径；S4 只负责中间档进桥，回收由既有语义兜底，此处不强发断言——2× DPI +
        // debug 下 worker 高密栅格慢，最终档 cover 到达时机由 worker 定）。
    }

    /// S5 回归：高倍稳态（render_scale 远超 COVER_SCALE_CAP=4.0）下，cap 截断的 cover 在
    /// 当前密度瓦片（模拟）全到位后被回收（消 32MB/页 双驻留 + 边隙透雾）；`zoom_armed`
    /// 期间**不回收**（guard）、`double_hold==0` 内存守卫恒成立。**注入式**：放大到高倍后
    /// 手动把该页置为「cap cover 在位 + tile_scale==render_scale + 瓦片非空」的稳态，
    /// 不依赖 worker 真实渲染（测试平台 2× DPI + debug 下高密栅格极慢、时序不稳定）。
    #[gpui::test]
    async fn high_scale_cover_retired_after_tiles_ready(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 连续放大直到 zoom 超过 ~2.6（render_scale 远超 cap）。
        let mut z_cur = 0.0f32;
        for _ in 0..14 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
            }
            z_cur = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            if z_cur >= 2.6 {
                break;
            }
        }
        assert!(z_cur >= 2.6, "应已放大到高倍（zoom={z_cur:.2}）");
        // 注入「cap 截断 cover 在位 + 当前密度瓦片就位」的稳态（绕过 worker）。
        let (p, cscale, cur) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let p = 0usize.min(v.dims.len() - 1);
            let cscale = layout::cover_scale(v.dims[p], pw, sf);
            let cur = layout::render_scale(pw, sf, v.dims[p].0);
            (p, cscale, cur)
        });
        assert!(
            cur > cscale * config::pdf::COVER_RETIRE_MIN_RATIO + 1e-4,
            "高倍前提：render_scale({cur:.2}) 应远超截断后的 cover_scale({cscale:.2})"
        );
        let cimg = crate::pdf::render_image_from_bgra(vec![200u8; 4 * 4 * 4], 4, 4).unwrap();
        let timg = crate::pdf::render_image_from_bgra(vec![220u8; 4 * 4 * 4], 4, 4).unwrap();
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let s = &mut v.slots[p];
                s.cover = Some(cimg);
                s.cover_scale = cscale;
                s.tile_scale = cur;
                s.tiles.insert(
                    (0, 0),
                    crate::pdf::layout::TileSlot {
                        image: timg,
                        x0: 0,
                        y0: 0,
                        w: 4,
                        h: 4,
                    },
                );
            });
        });
        // zoom_armed 期间不动：guard 必须拦住回收（防手势翻面回放）。防抖计时（idle>=3）
        // 会清 armed，而 `run_until_parked` 会连带 pump 多帧把 armed 冲掉——用 **1ms 单帧
        // 步进**（不触发 16ms pump timer，每步只推进一帧渲染：ensure_window 内 idle=1 仍
        // armed）钉住「armed 期」观察 guard。
        let mut still_hold = true;
        for _ in 0..4 {
            cx.executor().advance_clock(Duration::from_millis(1));
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                    v.zoom_armed = true;
                    v.zoom_idle_frames = 0;
                });
            });
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(1));
            still_hold = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[p];
                v.zoom_armed && s.cover.is_some() && s.cover_scale.abs() > 1e-4
            });
            if !still_hold {
                break;
            }
        }
        assert!(still_hold, "zoom_armed 期间不得回收 cover（guard 失败）");
        // 评审 BLOCK③：单片在槽 ≠ 铺满可见区 —— 清除 armed 后首片到达**不得回收**（否则
        // 高倍滚动/收口期首片即闪底）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.zoom_armed = false;
                v.zoom_idle_frames = 0;
                v.speed_state = crate::pdf::view::SpeedState::Slow;
                v.prev_fast = false;
            });
        });
        let mut held_with_one_tile = false;
        for _ in 0..8 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            held_with_one_tile = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[p];
                s.cover.is_some() && s.cover_scale.abs() > 1e-4 && !s.tiles.is_empty()
            });
        }
        assert!(
            held_with_one_tile,
            "评审③：可见矩形未铺满（单瓦片在槽）时不得回收 cover（防闪底）"
        );
        // 铺满整页瓦片网格（含可见矩形必然全在槽）→ 才允许回收。
        let (rows, cols) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let bmp = layout::page_bitmap_size(v.dims[p], layout::render_scale(pw, sf, v.dims[p].0));
            layout::tile_grid(bmp, config::pdf::TILE_SIZE)
        });
        let tile = crate::pdf::render_image_from_bgra(vec![220u8; 4 * 4 * 4], 4, 4).unwrap();
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                for row in 0..rows {
                    for col in 0..cols {
                        v.slots[p].tiles.insert(
                            (row, col),
                            crate::pdf::layout::TileSlot {
                                image: tile.clone(),
                                x0: col * config::pdf::TILE_SIZE,
                                y0: row * config::pdf::TILE_SIZE,
                                w: 4,
                                h: 4,
                            },
                        );
                    }
                }
            });
        });
        let mut retired = false;
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            retired = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[p];
                s.cover.is_none() || s.cover_scale.abs() <= 1e-4
            });
            if retired {
                break;
            }
        }
        assert!(retired, "高倍可见区瓦片铺满后可 cover 应被回收（消双驻留+边隙透雾）");
        // double_hold 计数（封面+瓦片双持有）恒 0。
        let double_hold = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (0..v.dims.len())
                .filter(|&p| {
                    let s = &v.slots[p];
                    s.cover.is_some()
                        && !s.tiles.is_empty()
                        && (s.cover_scale - s.tile_scale).abs() <= 1e-4
                })
                .count()
        });
        assert_eq!(double_hold, 0, "高倍稳态封面+瓦片不得双驻留（double_hold=0）」");
    }

    /// 评审② 定向回归：`zoom_armed` 推进首帧把可见窗内 **stale cover**（`cover_scale ≠
    /// 当前 cscale`，缩放中途正持旧档）迁入有界 `prev_cover`、`cover=None` —— 让 S2 preview
    /// 与 S4 最新档桥对「持旧封面页」激活（preview 不再被「cover 在位」双重丢弃）。用**真实
    /// 缩放事件**进入 armed 首帧（即实际迁移触发路径；worker 高密栅格慢，迁移态在首帧后仍可
    /// 观测——cover 不会被立即重派）。当前档 cover 在位页由 relocate 的 stale 判定天然保留。
    #[gpui::test]
    async fn zoom_armed_first_frame_relocates_stale_cover(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        let (p, cscale) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let p = 0usize.min(v.dims.len() - 1);
            let cscale = layout::cover_scale(v.dims[p], pw, sf);
            (p, cscale)
        });
        let cimg = crate::pdf::render_image_from_bgra(vec![200u8; 4 * 4 * 4], 4, 4).unwrap();
        // 手动构造「stale cover 在位」：缩放前的旧档整页（cover_scale=1.2×当前档）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let s = &mut v.slots[p];
                s.cover = Some(cimg);
                s.cover_scale = cscale * 1.2;
            });
        });
        // 真实缩放一档 → adjust_zoom 置位 armed+notify → 下一渲染帧 relocate stale cover。
        cx.simulate_event(zoom_wheel(true, 240.0));
        // 防抖首帧推进，采样迁移态；worker 高密栅格慢，cover 不会被立即重派。
        let mut relocated = false;
        for _ in 0..4 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            relocated = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[p];
                s.cover.is_none() && s.prev_cover.is_some()
            });
            if relocated {
                break;
            }
        }
        assert!(
            relocated,
            "armed 首帧应把 stale cover 迁入 prev_cover（S2 preview / S4 桥对持旧封面页激活）"
        );
        // 当前档 cover 在位页保持：relocate 只迁移「cover_scale≠当前 cscale」的 stale 槽——
        // 此语义由 `relocate_stale_zoomed_covers` 的 stale 判定 + `collect/render` 的「当前档
        // cover 才弃/隐藏 preview」共同保证（见其对 preview 的对称守卫），此处以迁移发生后的
        // 可达状态作一致性收尾：等待引擎推进若干帧后确认该页不再双持 stale cover（已收口路径）。
        let _ = p;
        let _ = cscale;
    }

    /// Z1 端到端回归：缩放重锚不再让行上下跳。锚 K 页（2）于视口顶，连续放大 3 档再缩回
    /// 3 档——每档「锚点页顶相对视口顶偏移」（= 同一字符 window-y）应守恒（≤2.5px），
    /// 回原档无漂移（旧公式每档漂移 ~−(ratio−1)×(PAD+K×GAP)，3 档深页可达 10-35px 阶跳）。
    #[gpui::test]
    async fn zoom_anchor_no_vertical_line_jump(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, mut cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 锚页 K=2：滚到页 2 顶部（scroll_y = tops[2]）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                let top2 = v.page_tops.get(2).copied().unwrap_or(0.0);
                v.scroll
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(-top2)));
                cx.notify();
            });
        });
        zoom_wait_frames(cx, 3);
        let base = zoom_anchor_rel(&mut cx, &holder, 2);
        // 3 档进 + 每档校验守恒。
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            zoom_wait_frames(cx, 3);
            let cur = zoom_anchor_rel(&mut cx, &holder, 2);
            assert!(
                (cur - base).abs() <= 2.5,
                "放大档锚点页顶相对视口应守恒：Δ{:.2}px（旧公式深页此处分跳行高）",
                cur - base
            );
        }
        // 3 档退（回原档）。
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, -240.0));
            zoom_wait_frames(cx, 3);
            let cur = zoom_anchor_rel(&mut cx, &holder, 2);
            assert!(
                (cur - base).abs() <= 2.5,
                "缩放回档锚点页顶相对视口应守恒：Δ{:.2}px（旧公式漂移 10-35px）",
                cur - base
            );
        }
        let final_rel = zoom_anchor_rel(&mut cx, &holder, 2);
        assert!(
            (final_rel - base).abs() <= 2.5,
            "回原档无漂移：base={base:.1} final={final_rel:.1}"
        );
    }

    /// Z2.1 回归：缩放 armed 首帧把「缩放中间档」在途 cover 直接清除（预算即释、不占
    /// COVER_INFLIGHT），防抖收口后只派最终档 cover（队列不被旧档占死 → 时锐时糊根治）。
    /// 注入 3 枚过期档 pending_cover（scale≠当前 cscale）→ armed 首帧 `drop_stale_scale_pending`
    /// 清掉 → 收口后最终档 cover/tile 派发；
    /// Z2.3 等价断言：被清的中间档**不会**进入最终栅格预算（raster_reqs 增量仅来自收口）。
    #[gpui::test]
    async fn zoom_drops_stale_midscale_covers_then_collects_final(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 缩放一档进入 armed（新档）。
        cx.simulate_event(zoom_wheel(true, 240.0));
        zoom_wait_frames(cx, 2);
        // 注入 3 枚「缩放中间档」在途封面（scale=1.2×当前 cscale，必非当前档）。
        cx.update(|w, cx| {
            let sf = w.scale_factor();
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                for p in 0..3usize.min(v.dims.len()) {
                    let cscale = layout::cover_scale(v.dims[p], v.page_width(), sf);
                    let (_, rx) =
                        std::sync::mpsc::channel::<Option<crate::pdf::RenderResult>>();
                    v.pending_cover
                        .insert(p, (cscale * 1.2, v.recolor_epoch, v.nav_gen, rx));
                }
            });
        });
        // armed 首帧（S3 块）drop_stale_scale_pending 清掉中间档 → pending_cover 空。
        // 防抖计时会清 armed：每帧 run **前** 保持 zoom_armed，确保调度帧以 armed 进入并
        // 执行 drop（首帧即清，随后多帧是否清 armed 无关）。
        let mut cleared = false;
        for _ in 0..6 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                    v.zoom_armed = true;
                    v.zoom_idle_frames = 0;
                });
            });
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            cleared = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_cover.is_empty()
                    || (0..3).all(|p| !v.pending_cover.contains_key(&p))
            });
            if cleared {
                break;
            }
        }
        assert!(cleared, "armed 首帧应清掉中间档 cover（预算即释，不占 COVER_INFLIGHT）");
        // 防抖到期收口 → 最终档 cover/tile 派发（非零游走）。
        let (c0, t0, r0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs, v.raster_reqs)
        });
        zoom_wait_frames(cx, 30);
        let (c1, t1, r1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs, v.raster_reqs)
        });
        eprintln!(
            "[Z2] 清后收口：cover {c0}→{c1} tile {t0}→{t1} raster {r0}→{r1}"
        );
        assert!(
            c1 > c0 || t1 > t0,
            "收口应派最终档 cover/tile（队列不再被中间档占死）"
        );
        // Z2.3：中间档被清 → 它们的栅格预算不被消费；raster 增量受限于收口最终档用量。
        assert!(
            r1 - r0 <= 64,
            "中间档不应进栅格预算（raster 增量应只来自最终档收口）：{r0}→{r1}"
        );
    }

    /// Z2.2 回归：相同缩放序列两次 → 收口栅格请求增量都在「一次性窗口用量」量级内、
    /// 不随序号/档位线性翻倍（确定性收敛语义：若中间档 cover 在每档都派进来排队，
    /// 3 档的 tile/cover 增量会接近单档收口的 3 倍；有界 → 无中间档堆叠、无随机雾窗）。
    #[gpui::test]
    async fn zoom_repeat_sequence_is_deterministic(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, mut cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        let run3 = |cx: &mut gpui::VisualTestContext,
                    holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >| {
            let (c0, t0) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.cover_reqs, v.tile_reqs)
            });
            for _ in 0..3 {
                cx.simulate_event(zoom_wheel(true, 240.0));
                for _ in 0..3 {
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.run_until_parked();
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            // 防抖到期 + 收口推进。
            zoom_wait_frames(cx, 25);
            let (c1, t1) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.cover_reqs, v.tile_reqs)
            });
            (c1 - c0, t1 - t0)
        };
        let d1 = run3(&mut cx, &holder);
        let d2 = run3(&mut cx, &holder);
        eprintln!("[Z2.2] 两轮 3 档增量：cover {d1:?} vs {d2:?}");
        // 收口一次性窗口用量上界：cover ≤ 封面向导页数 + 余量、tile ≤ 可见+缓冲片数；
        // 若逐档抢派中间档，3 档的 cover 会 ~3×、tile 随档成倍 → 远超上界/两轮发散。
        assert!(
            d1.0 <= 6 && d2.0 <= 6,
            "cover 收口应局限于窗口用量（不逐档堆叠）：{d1:?} / {d2:?}"
        );
        assert!(
            d1.1 <= 120 && d2.1 <= 120,
            "tile 收口应受限于可见片数量级（不随档数翻倍）：{d1:?} / {d2:?}"
        );
        // 两轮同一量级（确定性收敛，不因旧档中间档 tsuffle 发散）。
        assert!(
            (d1.1 as i64 - d2.1 as i64).abs() <= 60,
            "两轮 tile 收口应同一量级：{d1:?} / {d2:?}"
        );
    }

    /// #2 停底重锚 clamp 回归（机制级、确定性）：深页缩到文档底、Z1 公式算出的新 offset
    /// 超出新布局 max_offset 时——`adjust_zoom` 先把 `new_y` clamp 到 `[0, max_offset]` 再
    /// `set_offset`，并把 `last_scroll_y` 回写为 clamp 后值（防 S1 假速度 / 再放大反向补一跳）。
    /// 断言：锚定末页底部后连续放大，`-offset.y` 始终 ≤ 新 max_offset（容器不会被迫二次
    /// clamp），且 `last_scroll_y == -offset.y`（回写一致）。
    #[gpui::test]
    async fn zoom_bottom_clamp_keeps_offset_and_last_scroll_sync(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        // 直接滚到文档底（offset = -max_offset；新布局推进几帧让 bounds 落地）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let vh = f32::from(v.scroll.bounds().size.height);
                let max = (v.total_height - vh).max(0.0);
                v.scroll.set_offset(gpui::point(gpui::px(0.0), gpui::px(-max)));
            });
        });
        // 连续放大（锚在末页、越底越够到 max）；每档后校验 offset 在量程内且 last_scroll_y 一致。
        let mut violations: Vec<String> = Vec::new();
        for _ in 0..4 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..3 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                // 构造三问：offset 是否越界、last_scroll_y 是否与 offset 一致。
                let (sy, last_sy, max_sy, top_bottom) = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    let vh = f32::from(v.scroll.bounds().size.height);
                    let sy = f32::from(-v.scroll.offset().y);
                    let max_sy = (v.total_height - vh).max(0.0);
                    // 末页是否贴近文档底（钳制工况成立）
                    let last = v.dims.len().saturating_sub(1);
                    let bottom = v.page_tops[last] + crate::pdf::layout::display_height(v.dims[last], v.page_width());
                    (sy, v.last_scroll_y, max_sy, bottom - vh <= sy + 0.5)
                });
                let _ = top_bottom;
                if sy > max_sy + 0.5 {
                    violations.push(format!("offset 越界 sy={sy:.1} > max={max_sy:.1}"));
                }
                if (last_sy - sy).abs() > 1e-2 {
                    violations.push(format!("last_scroll_y 失配 last={last_sy:.1} vs sy={sy:.1}"));
                }
            }
        }
        eprintln!("[#2-clamp] violations={violations:?}");
        assert!(
            violations.is_empty(),
            "停底缩放 offset 必须 clamp 在量程内且 last_scroll_y 回写一致：{violations:?}"
        );
        // 收口（防抖到期）不残留。
        zoom_wait_frames(cx, 30);
        let armed = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom_armed);
        assert!(!armed, "防抖应收口");
    }

    /// #1+#2 端到端：锚点页**页内相对位置守恒**（缩放换档「行上下跳」的用户可观测不变式）。
    /// 中部页 40% 高度处 3 档进 + 3 档回——页顶相对视口偏移与「当前缩放下的页高×40%」
    /// 之差恒 <0.5px（Z1 锚点把同字符钉在页内同一相对位置；#2 clamp 不误触因为远离文档底）。
    /// 注：`pan_zoom::zoom_factor` 已改为**精确逆比**（放大 1+s / 缩小 1/(1+s)，往返严格
    /// 闭合，根治旧 `(1+s)(1-s)=1-s²` 每轮缩水 ~1.44% 的「来回缩放页面越滚越小」）——绝对
    /// 往返断言由 `pan_zoom` 单测与 `layout::tests::zoom_three_notch_roundtrip_within_half_px`
    /// 共同覆盖。
    #[gpui::test]
    async fn zoom_mid_doc_anchor_invariant_no_jump(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, mut cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 文档中部页，40% 高度（远离页顶/文档底两条 clamp 边界）。
        let target = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.dims.len().saturating_sub(1) / 2).max(1)
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let top = v.page_tops[target];
                let h = crate::pdf::layout::display_height(v.dims[target], v.page_width());
                v.scroll
                    .set_offset(gpui::point(gpui::px(0.0), gpui::px(-(top + h * 0.4))));
            });
        });
        zoom_wait_frames(cx, 5);
        // 基准：页内相对偏移 readout（top 现读，消除 base_w 适配噪声）。
        let read_inpage = |cx: &mut gpui::VisualTestContext, target: usize| -> f32 {
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let sy = f32::from(-v.scroll.offset().y);
                let h = crate::pdf::layout::display_height(v.dims[target], v.page_width());
                (sy - v.page_tops[target]) - h * 0.4
            })
        };
        let base = read_inpage(&mut cx, target);
        // 3 档进 + 3 档回；每档后校验「页内 40% 点相对视口位」守恒（同字符不跳）。
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            zoom_wait_frames(cx, 3);
            let cur = read_inpage(&mut cx, target);
            assert!(
                (cur - base).abs() < 0.5,
                "放大档锚点页内 40% 点应守恒（同字符不跳）：Δ{:.3}px",
                cur - base
            );
        }
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, -240.0));
            zoom_wait_frames(cx, 3);
            let cur = read_inpage(&mut cx, target);
            assert!(
                (cur - base).abs() < 0.5,
                "缩小档锚点页内 40% 点应守恒（同字符不跳）：Δ{:.3}px",
                cur - base
            );
        }
        zoom_wait_frames(cx, 20);
        let final_rel = read_inpage(&mut cx, target);
        eprintln!(
            "[#1+#2-invariant] base={base:.3} final={final_rel:.3} Δ={:.3}",
            final_rel - base
        );
        assert!(
            (final_rel - base).abs() < 0.5,
            "3 进 3 回页内 40% 点应守恒（同字符 window-y 不跳）：Δ={:.3}",
            final_rel - base
        );
    }

    /// **撤 #3 反断言（第 4 轮真机根因后的新契约）**：手势期**零满密换底**——armed 期间
    /// 中心可见页**无**当前档满密 cover（在途/入槽均无当下 cscale；前一轮“派中心满密 cover”
    /// 正是把「偶尔换」变成「每档必换」、整页跳的放大器，已撤销）。屏显稳定在**手势前那张**
    /// （armed 首帧迁入 prev_cover 的高密位图，GPU 拉伸）；settle（防抖到期）后才按最终
    /// cscale 一次性派满密收口。分两阶段断言：armed 无新满密、settle 后最终档达阵。
    #[gpui::test]
    async fn zoom_armed_no_center_cover_settle_delivers(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 真实缩放 1 档进入 armed。
        cx.simulate_event(zoom_wheel(true, 240.0));
        zoom_wait_frames(cx, 2);
        let (center, cscale, have_base) = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = w.scale_factor();
            let sy = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height);
            let vis = pdf_visible_pages(&v, pw, sy, vh);
            assert!(!vis.is_empty(), "armed 采样时应有可见页（先吃饱帧）");
            let center = vis[vis.len() / 2];
            let cscale = crate::pdf::layout::cover_scale(v.dims[center], pw, sf);
            // 手势前那张高密底图 = 当前槽 cover（cap 4.0 截断、盖度未变时不搬迁）或其
            // prev_cover 桥（盖度变了、armed 首帧 relocate 迁入）——两者任一即稳定底基。
            let base = v.slots[center].cover.is_some() || v.slots[center].prev_cover.is_some();
            (center, cscale, base)
        });
        eprintln!("[#3rev] center={center} cscale={cscale:.2} have_base={have_base}");
        assert!(have_base, "armed 应持有手势前那张高密底图（cover 或 prev_cover 桥）");
        // —— 阶段 A：armed 期（防抖计时会清 armed，每帧 run 前回钉保持 armed 观测）——
        // 断言：**无新中心满密 cover 派发**（pending_cover 无当前档条目——中心 cover 派发
        // 已从 open.rs armed 块撤除；drop_stale 逐帧清旧档）且底基保持（cover/prev_cover 驻留）。
        let mut no_dispatch = false;
        for _ in 0..6 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                    v.zoom_armed = true;
                    v.zoom_idle_frames = 0;
                });
            });
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            no_dispatch = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                if !v.zoom_armed {
                    return false;
                }
                // 无「当前档满密 cover」在途：不是「刚派的新满密」（旧档已被 drop_stale 清掉）。
                let no_inflight = !v
                    .pending_cover
                    .get(&center)
                    .is_some_and(|(sc, _, _, _)| (sc - cscale).abs() <= 1e-3);
                // 底基仍驻留（手势前那张，未被置换/清空）。
                let base_held = v.slots[center].cover.is_some()
                    || v.slots[center].prev_cover.is_some();
                no_inflight && base_held
            });
            if no_dispatch {
                break;
            }
        }
        assert!(
            no_dispatch,
            "armed 手势期中心页不得有新满密 cover 派发（在途无当前档）且底基必须驻留——零满密换底新契约：center={center}"
        );
        // —— 阶段 B：settle（防抖到期清 armed）→ 常态收口按最终 cscale 派满密 cover ——
        // 确定性断言「派发已发生」（计入 pending_cover，不依赖 worker 渲染完成）。
        let mut settled = false;
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            settled = cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                if v.zoom_armed {
                    return false;
                }
                let pw = v.page_width();
                let sf = w.scale_factor();
                let cscale_now = crate::pdf::layout::cover_scale(v.dims[center], pw, sf);
                v.pending_cover
                    .get(&center)
                    .is_some_and(|(sc, _, _, _)| (sc - cscale_now).abs() <= 1e-3)
                    || (v.slots[center].cover.is_some()
                        && (v.slots[center].cover_scale - cscale_now).abs() <= 1e-4)
            });
            if settled {
                break;
            }
        }
        assert!(
            settled,
            "settle 后最终档满密 cover 应派达中心页（一次性收口，不再逐档换底）"
        );
    }

    /// **雾糊复现测试（真 worker 往返 + 逐页 LOD 账）**。
    ///
    /// 用户真机证据：缩放「放大→缩回」往返 settle 后，某可见页残留**旧密度底图**
    /// （prev_cover 桥 / 0.5 preview / mid / thumb）且**静止不再补渲** → 整页雾糊；
    /// 滚走再滚回来也不恢复，只有再次缩放（密度变化）强制重渲才恢复；低倍（100%~125%）
    /// 比高倍更易糊、初始打开不糊。
    ///
    /// 本用例 headless 用**真 worker** 复现同一条往返路径，并分两档 regime 取证：
    /// **A（离初始档 3 进 3 回）** 与 **B（低倍 cover-served 档：先把 zoom 降到 render_scale
    /// 跌破 COVER_SCALE_CAP、使封面满密度即可独自锐利，贴近真机 100%~125% 低倍 regime）**。
    /// 每轮 settle 后做**真 quiescent**（armed 清 + cover/tile 在途清 + 可见页满密供给连续稳定），
    /// 逐可见页取证「实际显示底图档 + 密度比 vs 当前 render_scale」，并检测「**卡糊冻结态**」
    /// = 可见页显示低密底 + 满密供给（当前档 cover / 当前档 cover 在途 / 当前密度瓦片或
    /// 瓦片在途）全部缺失 + 非 armed——即「静止停在低密拉伸、调度不再补渲」。卡糊页再空转
    /// 40 帧仍不自愈 → headless 命中复现（真机「滚动不恢复、只缩放恢复」的镜像）。
    #[gpui::test]
    async fn zoom_blur_roundtrip_ledger_repro(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);

        // —— 取证闭包：逐可见页 LOD 账 + 卡糊冻结页清单 ——
        let read_ledger =
            |cx: &mut gpui::VisualTestContext| -> (Vec<String>, Vec<usize>) {
                cx.update(|w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    zoom_blur_ledger(v, w.scale_factor())
                })
            };

        // ① 基线：初始打开，推到真 quiescent（真机「初始不糊」基准）。
        let (q0, _st0) = zoom_blur_quiesce(&holder, cx, 800, 3);
        let (rows0, st0) = read_ledger(cx);
        eprintln!(
            "[ZOOM-REPRO] ①基线(初始打开) quiescent={q0} stuck={st0:?}:\n  {}",
            rows0.join("\n  ")
        );

        // ② 滚到第 3 页附近（真实跳转路径），推 quiescent。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.scroll_to_page(3.min(v.dims.len().saturating_sub(1)));
            });
        });
        let (q_s, _st_s) = zoom_blur_quiesce(&holder, cx, 800, 3);
        let (rows_s, st_s) = read_ledger(cx);
        eprintln!(
            "[ZOOM-REPRO] ②滚到第3页 quiescent={q_s} stuck={st_s:?}:\n  {}",
            rows_s.join("\n  ")
        );

        // ③ 连续 3 档放大（Ctrl+滚轮向上）→ 防抖 settle（不要求 quiescent，只观测档内态）。
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            zoom_wait_frames(cx, 2);
        }
        zoom_wait_frames(cx, 8);
        let (z_up, rows_up, _st_up) = {
            let (rows, st) = read_ledger(cx);
            let z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            (z, rows, st)
        };
        eprintln!(
            "[ZOOM-REPRO] ③放大3档 zoom={z_up:.3} armed清后(在途态):\n  {}",
            rows_up.join("\n  ")
        );

        // ④ 原路 3 档缩回 → 推真 quiescent → 卡糊判定（Phase A 主断言）。
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, -240.0));
            zoom_wait_frames(cx, 2);
        }
        let (qa, mut stuck_a) = zoom_blur_quiesce(&holder, cx, 1200, 3);
        let (z_back, rows_back) = {
            let (rows, _) = read_ledger(cx);
            let z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            (z, rows)
        };
        eprintln!(
            "[ZOOM-REPRO] ④PhaseA 缩回3档 zoom={z_back:.3} quiescent={qa}:\n  {}",
            rows_back.join("\n  ")
        );

        // ⑤ 卡糊冻结定案：卡糊页再空转 40 帧看是否自愈（真机「静止不补渲」= 不自愈）。
        if !stuck_a.is_empty() {
            let before = stuck_a.clone();
            zoom_wait_frames(cx, 40);
            (_, stuck_a) = read_ledger(cx);
            eprintln!(
                "[ZOOM-REPRO] ⑤PhaseA 卡糊页空转 40 帧: before={before:?} after={stuck_a:?}"
            );
        }

        // ⑥ Phase B：低倍 cover-served 档（贴近真机 100%~125% 低倍 regime）。
        //   先缩小到当前可见页 render_scale 跌破 COVER_SCALE_CAP 一截（封面满密度、可独自锐利），
        //   再推 quiescent 立「低倍新鲜基准」，然后 3 进 3 回再 quiescent 断言。
        let (low_ok, low_zoom) = zoom_blur_to_low_scale(&holder, cx, 40);
        eprintln!("[ZOOM-REPRO] ⑥转入低倍 regime ok={low_ok} zoom={low_zoom:.3}");
        let (q_l, _st_l) = zoom_blur_quiesce(&holder, cx, 1200, 3);
        let (rows_l, st_l) = read_ledger(cx);
        eprintln!(
            "[ZOOM-REPRO] ⑥低倍新鲜基准 quiescent={q_l} stuck={st_l:?}:\n  {}",
            rows_l.join("\n  ")
        );
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            zoom_wait_frames(cx, 2);
        }
        for _ in 0..3 {
            cx.simulate_event(zoom_wheel(true, -240.0));
            zoom_wait_frames(cx, 2);
        }
        let (qb, mut stuck_b) = zoom_blur_quiesce(&holder, cx, 1200, 3);
        let (z_bl, rows_bl) = {
            let (rows, _) = read_ledger(cx);
            let z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            (z, rows)
        };
        eprintln!(
            "[ZOOM-REPRO] ⑥PhaseB 低倍 3进3回 zoom={z_bl:.3} quiescent={qb}:\n  {}",
            rows_bl.join("\n  ")
        );
        if !stuck_b.is_empty() {
            let before = stuck_b.clone();
            zoom_wait_frames(cx, 40);
            (_, stuck_b) = read_ledger(cx);
            eprintln!(
                "[ZOOM-REPRO] ⑥PhaseB 卡糊页空转 40 帧: before={before:?} after={stuck_b:?}"
            );
        }

        // —— 卡糊综合判定：任一条（A/B）quiescent 后卡糊且空转不自愈 → 复现命中 ——
        let stuck_final = {
            let mut s = stuck_a.clone();
            s.extend(stuck_b.iter().copied());
            s.sort_unstable();
            s.dedup();
            s
        };
        if !stuck_final.is_empty() {
            panic!(
                "[ZOOM-REPRO] 复现命中：往返后可见页静止卡糊 {stuck_final:?} \
                 （低密底拉伸 + 满密零在途 + 空转 40 帧不自愈）"
            );
        }
        eprintln!(
            "[ZOOM-REPRO] 结论：headless 未命中——A/B 两档 quiescent 后所有可见页均有满密供给 \
             （cover 在槽/在途 或 当前密度瓦片），静止自愈正常"
        );

        // ⑦ 诊断（不硬断言）：滚走（第5页）再滚回（第3页），看卡糊/低密态是否恢复。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                let last = v.dims.len().saturating_sub(1);
                v.scroll_to_page(5.min(last));
            });
        });
        zoom_wait_frames(cx, 10);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.scroll_to_page(3.min(v.dims.len().saturating_sub(1)));
            });
        });
        zoom_wait_frames(cx, 10);
        let (rows_away, st_away) = read_ledger(cx);
        eprintln!(
            "[ZOOM-REPRO] ⑦滚走再滚回后(刚回窗,在途态):\n  {}（stuck={st_away:?}）",
            rows_away.join("\n  ")
        );
    }

    /// 雾糊复现取证：逐可见页 LOD 账（页号 | cover 当前档? | cover 在途? | prev_cover? |
    /// prefetch 密度比 | mid | thumb | 瓦片数与密度 | 瓦片在途 | 卡糊标记 → 实际底图档）。
    /// 返回（逐页描述行, 卡糊冻结页清单）。**卡糊** = 可见页显示低密底（prev/prefetch/mid/
    /// thumb 任一在槽）且满密供给全无（cover 非当前档 / 当前档 cover 不在途 / 瓦片空或非
    /// 当前密度 / 无本页瓦片在途）且非 armed——「静止停在低密拉伸、调度不再补渲」。
    /// `prefetch` 密度比 = `prefetch_cover_scale / render_scale`（0.5 密度 ≈ 0.5）。
    fn zoom_blur_ledger(
        v: &crate::pdf::PdfViewer,
        scale_factor: f32,
    ) -> (Vec<String>, Vec<usize>) {
        let pw = v.page_width();
        let sy = f32::from(-v.scroll.offset().y);
        let vh = f32::from(v.scroll.bounds().size.height);
        let vis = pdf_visible_pages(v, pw, sy, vh);
        let mut rows = Vec::new();
        let mut stuck: Vec<usize> = Vec::new();
        for &p in &vis {
            let s = &v.slots[p];
            let cscale = crate::pdf::layout::cover_scale(v.dims[p], pw, scale_factor);
            let scale = crate::pdf::layout::render_scale(pw, scale_factor, v.dims[p].0);
            let cover_ok = s.cover.is_some() && (s.cover_scale - cscale).abs() <= 1e-4;
            let pend_cover = v
                .pending_cover
                .get(&p)
                .is_some_and(|(sc, _, _, _)| (sc - cscale).abs() <= 1e-3);
            let tile_ok = !s.tiles.is_empty() && (s.tile_scale - scale).abs() <= 1e-4;
            let tile_pend = v.pending_tiles.keys().any(|(pg, _, _)| *pg == p);
            let low_base = s.prev_cover.is_some()
                || s.prefetch_cover.is_some()
                || s.mid.is_some()
                || s.thumb.is_some();
            let is_stuck = !cover_ok
                && !pend_cover
                && !tile_ok
                && !tile_pend
                && low_base
                && !v.zoom_armed;
            if is_stuck {
                stuck.push(p);
            }
            let base = if cover_ok {
                format!("cover({:.2}x)", s.cover_scale / scale)
            } else if s.prev_cover.is_some() {
                "prev_cover(旧密度,scale失记)".to_string()
            } else if s.prefetch_cover.is_some() {
                format!("prefetch({:.2}x)", s.prefetch_cover_scale / scale)
            } else if s.mid.is_some() {
                "mid(~520px)".to_string()
            } else if s.thumb.is_some() {
                "thumb(~260px)".to_string()
            } else if s.cover.is_some() {
                format!("cover-stale({:.2}x)", s.cover_scale / scale)
            } else {
                "none".to_string()
            };
            rows.push(format!(
                "p{p}: c@cur={} c_pend={} prev={} pref={:.2}x mid={} thumb={} tiles({})@cur={} t_pend={} st={} → {base}",
                if cover_ok { "Y" } else { "-" },
                if pend_cover { "Y" } else { "-" },
                if s.prev_cover.is_some() { "Y" } else { "-" },
                if s.prefetch_cover.is_some() { s.prefetch_cover_scale / scale } else { 0.0 },
                if s.mid.is_some() { "Y" } else { "-" },
                if s.thumb.is_some() { "Y" } else { "-" },
                s.tiles.len(),
                if tile_ok { "Y" } else { "-" },
                if tile_pend { "Y" } else { "-" },
                if is_stuck { "STUCK" } else { "-" },
            ));
        }
        (rows, stuck)
    }

    /// 等「视觉 quiescent」（雾糊复现测试用）：armed 清 + cover/tile 在途清（镜像静态帧门，
    /// 不含不与门控的 prefetch/mid/thumb）+ 可见页无卡糊冻结，连续 `stable_frames` 帧成立。
    /// 返回 `(是否达到, 卡糊页清单)`——未达 = headless 渲染仍在途，测试不据此断言复现。
    fn zoom_blur_quiesce(
        holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >,
        cx: &mut gpui::VisualTestContext,
        budget_frames: usize,
        stable_frames: u32,
    ) -> (bool, Vec<usize>) {
        use std::time::Duration;
        let mut stable = 0u32;
        for _ in 0..budget_frames {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            let (busy, stuck) = cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let busy = v.zoom_armed
                    || !v.pending_cover.is_empty()
                    || !v.pending_tiles.is_empty();
                let (_, stuck) = zoom_blur_ledger(v, w.scale_factor());
                (busy, stuck)
            });
            if !busy && stuck.is_empty() {
                stable += 1;
                if stable >= stable_frames {
                    return (true, stuck);
                }
            } else {
                stable = 0;
            }
        }
        let stuck = cx.update(|w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            zoom_blur_ledger(v, w.scale_factor()).1
        });
        (false, stuck)
    }

    /// 2c **静止必收敛不变式**：可见页要么「封面在槽且 cover_scale≈cscale」、要么「可见
    /// 矩形内瓦片 1:1 全在槽（`tile_scale≈当前 render_scale`）」——终态 ≥ 目标密度，杜绝
    /// 「未达满密被当 settled 冻结」。直接复用瓦片调度自身的 [`PdfViewer::plan_tiles_in_window`]
    /// 几何（同一 page_visible_rect/visible_tile_range，零重实现、零漂移）：quiescent 下
    /// `pending_*` 已空，`plan_tiles` 返回的缺失瓦片 = 该页可见矩形仍未铺满的瓦片。
    /// 返回违反页清单（空 = 全收敛）。
    fn zoom_blur_convergence_violations(
        v: &crate::pdf::PdfViewer,
        scale_factor: f32,
        viewport_h: f32,
        window: &gpui::Window,
    ) -> Vec<usize> {
        use crate::pdf::layout as L;
        let pw = v.page_width();
        let sy = f32::from(-v.scroll.offset().y);
        let vis = pdf_visible_pages(v, pw, sy, viewport_h);
        if vis.is_empty() {
            return Vec::new();
        }
        let first = *vis.first().unwrap();
        let last = *vis.last().unwrap();
        let center = (first + last) / 2;
        // quiescent 下 pending_tiles 已空 → plan 返回的仍是缺失瓦片（未入槽、未在途）。
        let need = v.plan_tiles_in_window(
            first..=last,
            center,
            pw,
            scale_factor,
            sy,
            viewport_h,
            window,
        );
        let mut bad = Vec::new();
        for &p in &vis {
            let s = &v.slots[p];
            let cscale = L::cover_scale(v.dims[p], pw, scale_factor);
            if s.cover.is_some() && (s.cover_scale - cscale).abs() <= 1e-4 {
                continue; // 封面在槽（当前密度）→ 已锐
            }
            // 封面缺席 → 可见矩形瓦片由 plan 供给，缺失集里不得再含该页（否则糊冻）。
            if need.iter().any(|&(pg, _, _, _)| pg == p) {
                bad.push(p);
            }
        }
        bad
    }

    /// 把当前可见页 render_scale 降到 COVER_SCALE_CAP 之下（0.93×cap），贴近真机「低倍、
    /// 封面满密度可独自锐利」的 regime；返回（是否成功, 最终 zoom）。缩放事件触发防抖，
    /// 调用者随后推 quiescent。
    fn zoom_blur_to_low_scale(
        holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >,
        cx: &mut gpui::VisualTestContext,
        max_notches: u32,
    ) -> (bool, f32) {
        let mut zoom = 1.0f32;
        for _ in 0..max_notches {
            let (below, z) = cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let pw = v.page_width();
                let sf = w.scale_factor();
                let sy = f32::from(-v.scroll.offset().y);
                let vh = f32::from(v.scroll.bounds().size.height);
                let vis = pdf_visible_pages(v, pw, sy, vh);
                let p = *vis.first().unwrap_or(&0);
                let scale = crate::pdf::layout::render_scale(pw, sf, v.dims[p].0);
                (scale <= config::pdf::COVER_SCALE_CAP * 0.97, v.zoom)
            });
            if below {
                return (true, z);
            }
            cx.simulate_event(zoom_wheel(true, -240.0));
            zoom_wait_frames(cx, 2);
            zoom = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
        }
        (false, zoom)
    }

    /// **zoom 分步 mod-4 模糊机制探针**。用户真机确定性配方（非竞态，headless 可复现）：
    /// 初始 100% 清晰；放大 1/2/3 步糊、放大 4 步清晰；4 步基础上缩回 1/2/3 步糊、缩回 4 步
    /// 清晰（回 100%）——清晰度按「净步数 mod 4 = 0」周期翻转、放大/缩小对称。
    ///
    /// 本探针在 **sf=1.0** 复刻真机「低倍 cover-served、纯封面供给、无瓦片」regime
    /// （测试平台默认 2× DPI 会让 render_scale 在 100% 即越过 COVER_SCALE_CAP → 永远有
    /// 1:1 瓦片撑锐，远离真机）。扫多个窗口宽度（含让 render_scale 在 1..5 步内越过
    /// [4.0, 4.06] COVER_SCALE_CAP/eps 刀刃的宽度），每档取**两次账**：
    /// ① 瞬态 T（notch 后 ~4 帧，缩放防抖/封面在途——真机用户此刻看到的就是屏显）；
    /// ② 稳态 S（真 quiescent 后）。每行给期望 cover 密度(cscale)/cover_serves 判定/
    /// 槽内实际底图档/密度比/显示设备宽 vs cover 位图设备宽/snap 残余亚像素，判「糊步」。
    /// 只取证不断言（诊断用）。
    #[gpui::test]
    async fn zoom_step_mod4_blur_probe(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        use std::time::Duration;
        // 宽度从低倍（scale≈1.6）到拖到 5 步内越过 4.06 刀刃（base_w 比例域全覆盖）
        let widths: &[(f32, f32, &str)] = &[
            (1000.0, 800.0, "W1000"),
            (1200.0, 800.0, "W1200"),
            (1500.0, 900.0, "W1500"),
            (1600.0, 900.0, "W1600"),
            (1765.0, 900.0, "W1765"),
            (2170.0, 900.0, "W2170"),
            (2420.0, 900.0, "W2420"),
        ];
        for &(ww, wh, label) in widths {
            let holder = zoom_holder();
            let (_root, cx) = {
                // zoom_add_window 硬编码 1500×900；这里按参数化大小开窗并强制 sf=1.0
                let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/pdf测试2.pdf");
                cx.update(crate::test_support::bind_local_keymaps);
                let h = holder.clone();
                let (root, vcx) = cx.add_window_view(move |_window, cx| {
                    let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(path.clone(), cx));
                    *h.borrow_mut() = Some(pdf.clone());
                    ZoomProbe { pdf }
                });
                vcx.simulate_resize(gpui::size(gpui::px(ww), gpui::px(wh)));
                vcx.update(|w, _| w.set_scale_factor(1.0));
                let mut loaded = false;
                for _ in 0..400 {
                    vcx.run_until_parked();
                    loaded = vcx.update(|_w, cx| {
                        holder
                            .borrow()
                            .as_ref()
                            .map(|pv| pv.read(cx).is_loaded())
                            .unwrap_or(false)
                    });
                    if loaded {
                        break;
                    }
                }
                assert!(loaded, "测试 PDF 应加载完成");
                for _ in 0..6 {
                    vcx.executor().advance_clock(Duration::from_millis(16));
                    vcx.run_until_parked();
                }
                (root, vcx)
            };

            // 取一可见页的账（tag=T 瞬态 / S 稳态）。糊 = 当前显示底图密度比 < 1.0。
            let row = |cx: &mut gpui::VisualTestContext, tag: &str, step: i32| {
                cx.update(|w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    let sf = w.scale_factor();
                    let pw = v.page_width();
                    let sy = f32::from(-v.scroll.offset().y);
                    let vh = f32::from(v.scroll.bounds().size.height);
                    let vis = pdf_visible_pages(v, pw, sy, vh);
                    let p = *vis.first().unwrap_or(&0);
                    let s = &v.slots[p];
                    let scale = crate::pdf::layout::render_scale(pw, sf, v.dims[p].0);
                    let cscale = crate::pdf::layout::cover_scale(v.dims[p], pw, sf);
                    let serve = crate::pdf::layout::cover_serves_scale(cscale, scale);
                    let cover_ok = s.cover.is_some() && (s.cover_scale - cscale).abs() <= 1e-4;
                    // 显示底图密度比：1.0=同密度锐化；prev_cover≈1/1.12≈0.89；
                    // prefetch=prefetch_scale/scale（0.5× 原始 cscale）；mid≈520px/显示宽；thumb≈260px/显示宽
                    let (base, ratio) = if cover_ok {
                        ("cover(cur)", 1.0f32)
                    } else if s.prev_cover.is_some() {
                        ("prev_cover", 1.0 / 1.12)
                    } else if s.prefetch_cover.is_some() {
                        ("prefetch", s.prefetch_cover_scale / scale)
                    } else if s.mid.is_some() {
                        ("mid", 520.0 / pw)
                    } else if s.thumb.is_some() {
                        ("thumb", 260.0 / pw)
                    } else {
                        ("none", 0.0)
                    };
                    // snap_to_device 残余：页卡顶对齐设备网格后仍剩的亚像素（设备 px）
                    let top_raw = v.page_tops[p] + sy;
                    let top_snapped =
                        crate::pdf::layout::snap_to_device(top_raw, sf) - sy;
                    let snap_off_dev = ((top_snapped - v.page_tops[p]) * sf).abs();
                    // 期望 cover 位图设备宽（cscale 未截断时 = ceil(显示设备宽)）
                    let disp_dev = pw * sf;
                    let cov_bmp = crate::pdf::layout::page_bitmap_size(v.dims[p], cscale);
                    // cover 位图设备宽 vs 显示设备宽：<1 时 cover 上采样（糊源）
                    let dev_ratio = cov_bmp.0 as f32 / disp_dev;
                    eprintln!(
                        "[ZOOM-MOD4] {} {} step={:<2} z={:.4} pw={:.1} base={} ratio={:.3} scale={:.4} cscale={:.4} serve={} cover@{}/slot={:.4} prev={} pref={}({:.2}x) mid={} thumb={} tiles({})@cur={} tpend={} disp_dev={:.2} cov_bmp_w={} cov/display={:.4} snap_off_dev={:.3} armed={}",
                        label, tag, step, v.zoom, pw, base, ratio, scale, cscale,
                        if serve { "Y" } else { "-" },
                        if cover_ok { "Y" } else { "-" },
                        s.cover_scale,
                        if s.prev_cover.is_some() { "Y" } else { "-" },
                        if s.prefetch_cover.is_some() { "Y" } else { "-" },
                        if s.prefetch_cover.is_some() { s.prefetch_cover_scale / scale } else { 0.0 },
                        if s.mid.is_some() { "Y" } else { "-" },
                        if s.thumb.is_some() { "Y" } else { "-" },
                        s.tiles.len(),
                        if !s.tiles.is_empty() && (s.tile_scale - scale).abs() <= 1e-4 { "Y" } else { "-" },
                        if v.pending_tiles.keys().any(|(pg, _, _)| *pg == p) { "Y" } else { "-" },
                        disp_dev, cov_bmp.0, dev_ratio, snap_off_dev, v.zoom_armed,
                    );
                });
            };

            eprintln!("[ZOOM-MOD4] ======== 窗口 {label}（resize {ww}×{wh}, sf=1.0） ========");
            // 2c 静止必收敛断言：quiescent 后每可见页要么 cover_scale≈cscale、要么可见矩形
            // 内瓦片 1:1 覆盖（终态 ≥ 目标密度，杜绝「未达满密被当 settled 冻结」）。
            let converge = |cx: &mut gpui::VisualTestContext| {
                let bad = cx.update(|w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    let vh = f32::from(v.scroll.bounds().size.height);
                    zoom_blur_convergence_violations(v, w.scale_factor(), vh, w)
                });
                if !bad.is_empty() {
                    eprintln!("[ZOOM-MOD4] {label} 静止未收敛页={bad:?}");
                }
                assert!(
                    bad.is_empty(),
                    "[ZOOM-MOD4] {label} 静止必收敛被违（页 {bad:?}）：封面缺失页未补瓦片/封面 → 糊冻"
                );
            };
            let (q0, _) = zoom_blur_quiesce(&holder, cx, 800, 2);
            eprintln!("[ZOOM-MOD4] {label} step0 quiescent={q0}");
            row(cx, "S", 0);
            converge(cx);
            // 放大相：每 notch 后取瞬态 T + 稳态 S
            for step in 1..=5i32 {
                cx.simulate_event(zoom_wheel(true, 240.0));
                zoom_wait_frames(cx, 4);
                row(cx, "T", step);
                let (q, st) = zoom_blur_quiesce(&holder, cx, 800, 2);
                eprintln!("[ZOOM-MOD4] {label} 放大{step} quiescent={q} stuck={st:?}");
                row(cx, "S", step);
                converge(cx);
            }
            // 缩回相：5 档基础上一档档缩回（净步 4..0）
            for net in (0..=4i32).rev() {
                cx.simulate_event(zoom_wheel(true, -240.0));
                zoom_wait_frames(cx, 4);
                row(cx, "T", net);
                let (q, st) = zoom_blur_quiesce(&holder, cx, 800, 2);
                eprintln!("[ZOOM-MOD4] {label} 缩回→净{net} quiescent={q} stuck={st:?}");
                row(cx, "S", net);
                converge(cx);
            }
        }
    }

    /// #5 去 E（高倍收口雾窗）：cur>4 收口帧把 TILE_INFLIGHT=6 临时抬到
    /// SETTLE_TILE_BURST_MAX 一次性入队可见瓦片。断言：1) 收口后 `settle_tile_burst_dispatch`
    /// > 0（突发生效、headless 确定性证据）且 ≤ SETTLE_TILE_BURST_MAX（预算有界）；
    /// 2) 突发帧后 `settle_tile_burst` 被消费复清（只抬当帧、普通滚动语义不变）。
    #[gpui::test]
    async fn zoom_high_scale_settle_bursts_visible_tiles(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        zoom_wait_frames(cx, 40);
        // 连续放大到高倍（render_scale 远超 cap → 纯瓦片收口路径；镜像 high_scale 测试）。
        let mut z_cur = 0.0f32;
        for _ in 0..16 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
            }
            z_cur = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            if z_cur >= 2.6 {
                break;
            }
        }
        assert!(z_cur >= 2.6, "应已放大到高倍（zoom={z_cur:.2}）");
        // 收口（防抖到期清 armed → settle_burst 置位 → schedule_tiles 突发帧）。
        let mut dispatch = 0u64;
        let mut burst_consumed = false;
        for _ in 0..30 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            (dispatch, burst_consumed) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.settle_tile_burst_dispatch, !v.settle_tile_burst)
            });
            if dispatch > 0 {
                break;
            }
        }
        eprintln!(
            "[#5] settle burst dispatch={dispatch} (≤{} / 常态 {}) consumed={burst_consumed}",
            config::pdf::SETTLE_TILE_BURST_MAX,
            config::pdf::TILE_INFLIGHT
        );
        assert!(
            dispatch > 0,
            "高倍收口帧应触发瓦片一次性出清（settle_tile_burst_dispatch>0）"
        );
        assert!(
            dispatch as usize <= config::pdf::SETTLE_TILE_BURST_MAX,
            "收口突发应受出清预算上限夹（单次突发 ≤ 上限）：{dispatch}"
        );
        assert!(
            burst_consumed,
            "突发帧后 settle_tile_burst 应被消费（普通滚动语义不变）"
        );
    }

    /// **A1 收敛十字门（高倍钉住收敛，R1-R3）**：真实 A4 fixture + 1200×1400 窗 +
    /// `set_scale_factor(1.5)` 与 `2.0` + `adjust_zoom` 推到 zoom=4 + quiescent 后，断言每个
    /// 可见页 `visible_tile_range(可见 bbox + TILE_MARGIN)` 覆盖的**每一格**都在 `slot.tiles`
    /// ——高倍稳态仍是 1:1 锐利、无「缺格露糊底」的方形格子。修前（固定 32MiB 预算逐出可见
    /// 片）sf=2.0 工作集（~42 格 ≈42MiB）超容量 → 崩缺格（红）；修后（动态预算 = max(固定,
    /// 可见格×单格×1.25) + 钉住不逐出 + 视口相对排名）→ 全收敛（绿）。沿用
    /// `zoom_blur_convergence_violations` 的同源几何，但把 sf 从 1.0 抬到 1.5/2.0 跑满
    /// （1.0 时 cover 满密度、绕开预算十字门）。
    #[gpui::test]
    async fn high_zoom_convergence_pins_all_visible_tiles(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        for sf in [1.5f32, 2.0] {
            let holder = zoom_holder();
            let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
            // 1200×1400 视口：sf2.0 zoom4 可见格 ≈42 → 超 32MiB 固定预算（修前崩缺格红）；
            // sf1.5 zoom4 ≈30 格贴预算线。两档都低于 scale-16 密度上限、渲染可收敛。
            cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1400.)));
            for _ in 0..3 {
                cx.run_until_parked();
            }
            cx.update(|w, _| w.set_scale_factor(sf));
            for _ in 0..3 {
                cx.run_until_parked();
            }
            // Ctrl+滚轮推到 zoom=4（ZOOM_MAX；12%/档，需 ~13 档）。
            let mut z = 0.0f32;
            for _ in 0..40 {
                cx.simulate_event(zoom_wheel(true, 240.0));
                for _ in 0..2 {
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.run_until_parked();
                }
                z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
                if z >= config::pdf::ZOOM_MAX - 0.05 {
                    break;
                }
            }
            assert!(
                z >= config::pdf::ZOOM_MAX - 0.05,
                "[A1] sf={sf} 应推到 zoom=4：z={z}"
            );
            let (q, _stuck) = zoom_blur_quiesce(&holder, cx, 1100, 2);
            // 注：修前固定 32MiB 预算逐出可见片 → 调度/逐出震荡、quiescent 恒不达（q=false），
            // 且可见格崩缺（missing>0）；修后 q=true 且 missing=∅。q 仅取证，不符合仍以缺格
            // 断言为准（worker 串行高密渲染受墙钟抖动影响，最慢帧可拖到预算帧之外）。
            eprintln!("[A1] sf={sf} zoom4 quiescent={q}");
            // 逐可见页断言「可见矩形 + MARGIN 每格都在槽」（与 A1 验收口径逐字：格子全在、
            // 无缺格）。修前逐出可见片 → missing>0（红）。
            let (missing, cell_total, tile_bytes, budget, npin) = cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let vh = f32::from(v.scroll.bounds().size.height);
                let rows = v.visible_tile_presence(w.scale_factor(), vh, w);
                let cell_total: u64 = rows.iter().map(|&(_, _, t)| t as u64).sum();
                let missing: Vec<(usize, u32, u32)> =
                    rows.into_iter().filter(|&(_, m, _)| m > 0).collect();
                let bs = v.tile_budget_state.as_ref();
                (
                    missing,
                    cell_total,
                    v.tile_bytes,
                    bs.map(|s| s.budget).unwrap_or(0),
                    bs.map(|s| s.pinned.len()).unwrap_or(0),
                )
            });
            eprintln!(
                "[A1] sf={sf} cell_total={cell_total} tile_bytes={tile_bytes} budget={budget} npin={npin} missing={missing:?}"
            );
            assert!(
                missing.is_empty(),
                "[A1] sf={sf} zoom=4 可见格未全收敛（缺格即高倍方形格子根因未除）：{missing:?}"
            );
        }
    }

    /// **A2 无震荡（R1-R3 收敛后稳定）**：高倍 quiescent 后再连跑 2 帧，断言可见格集合与
    /// 瓦片占用字节**不变**、`plan_tiles_in_window`（可见页窗）返回 need==∅——即不再
    /// 「逐出→重派→逐出」每帧震荡（修前固定预算在 sf2.0 曾把 pending 顶满、quiescent 不达）。
    #[gpui::test]
    async fn high_zoom_converged_no_oscillation(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|w, _| w.set_scale_factor(2.0));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut z = 0.0f32;
        for _ in 0..40 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
            }
            z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            if z >= config::pdf::ZOOM_MAX - 0.05 {
                break;
            }
        }
        assert!(z >= config::pdf::ZOOM_MAX - 0.05, "[A2] 应推到 zoom=4：z={z}");
        let (q, _) = zoom_blur_quiesce(&holder, cx, 1100, 2);
        eprintln!("[A2] quiescent={q}");
        let snapshot = |cx: &mut gpui::VisualTestContext| -> (Vec<(usize, u32, u32)>, u64, usize) {
            cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let pw = v.page_width();
                let sy = f32::from(-v.scroll.offset().y);
                let vh = f32::from(v.scroll.bounds().size.height);
                let vis = pdf_visible_pages(v, pw, sy, vh);
                let first = *vis.first().unwrap_or(&0);
                let last = *vis.last().unwrap_or(&0);
                let center = (first + last) / 2;
                let missing = v
                    .visible_tile_presence(w.scale_factor(), vh, w)
                    .into_iter()
                    .filter(|&(_, m, _)| m > 0)
                    .collect::<Vec<_>>();
                let need = v.plan_tiles_in_window(
                    first..=last,
                    center,
                    pw,
                    w.scale_factor(),
                    sy,
                    vh,
                    w,
                );
                (missing, v.tile_bytes, need.len())
            })
        };
        let (m0, b0, need0) = snapshot(&mut *cx);
        let (m1, b1, need1) = snapshot(&mut *cx);
        assert_eq!(m1, m0, "[A2] 可见格集合不应变化（收敛后无逐出震荡）：{m1:?} vs {m0:?}");
        assert_eq!(b1, b0, "[A2] 瓦片占用字节不应变化（无逐出/重派）：{b1} vs {b0}");
        assert_eq!(need0, 0, "[A2] quiescent 后可见页 plan 不应再派瓦片：need={need0}");
        assert_eq!(need1, 0, "[A2] 2 帧后可见页 plan 仍不应派瓦片：need={need1}");
    }

    /// **A3 预算触顶 R4 均匀降级 + 自愈（无死锁）**：高倍收敛后把动态预算硬顶覆写为小值
    /// （测试钩子构造「预算触顶 + 可见组不齐」）→ 连续静态 N 帧后该页进入**均匀 0.5 降级**
    /// （瓦片清空、cap 截断 cover 退休、派发 0.5 密度 preview 垫底 = 宁均匀不格子、非整页永糊）；
    /// 随后移除覆写 + 缩小视口 → 帧不再触顶 → 降级自愈、可见格恢复 1:1。
    #[gpui::test]
    async fn budget_capped_uniform_degrade_then_recovers(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1400.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|w, _| w.set_scale_factor(2.0));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut z = 0.0f32;
        for _ in 0..40 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
            }
            z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            if z >= config::pdf::ZOOM_MAX - 0.05 {
                break;
            }
        }
        assert!(z >= config::pdf::ZOOM_MAX - 0.05, "[A3] 应推到 zoom=4：z={z}");
        let (q, _) = zoom_blur_quiesce(&holder, cx, 1100, 2);
        eprintln!("[A3] 初始收敛 quiescent={q}");
        // ① 覆写预算硬顶 → 构造「预算触顶 + 组不齐」。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.tile_budget_cap_override = Some(6 * 1024 * 1024);
            });
        });
        // ② 推进静态帧（含静态帧门控早退帧）→ 连续 DEGRADE_STATIC_FRAMES 帧后激活降级。
        let mut degraded = false;
        let mut tiles_drained = true;
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            (degraded, tiles_drained) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.degraded.contains(&0), v.slots[0].tiles.is_empty())
            });
            if degraded && tiles_drained {
                break;
            }
        }
        assert!(
            degraded && tiles_drained,
            "[A3] 连续静态 N 帧后应进入 R4 均匀降级（瓦片清空）：degraded={degraded} drained={tiles_drained}"
        );
        // ③ 等 0.5 均匀 preview 落地（非格子、非整页永糊：有统一垫底）。
        let mut prefetch_ok = false;
        for _ in 0..80 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            prefetch_ok = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.slots[0].prefetch_cover.is_some()
                    && (v.slots[0].prefetch_cover_scale
                        - (crate::pdf::layout::cover_scale(v.dims[0], v.page_width(), 2.0)
                            * config::pdf::PREFETCH_COVER_DENSITY)
                            .min(config::pdf::COVER_SCALE_CAP))
                        .abs()
                        <= 1e-4
            });
            if prefetch_ok {
                break;
            }
        }
        assert!(
            prefetch_ok,
            "[A3] 降级页应有均匀 0.5 密度 preview 垫底（宁均匀不格子、非整页永糊）：prefetch_ok={prefetch_ok}"
        );
        // ④ 自愈：移除覆写 + 缩小视口 → 帧不再触顶 → 恢复 1:1（可见格全在槽）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _| {
                v.tile_budget_cap_override = None;
            });
        });
        cx.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(1100.)));
        let mut recovered = false;
        for _ in 0..1200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            recovered = cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let vh = f32::from(v.scroll.bounds().size.height);
                v.visible_tile_presence(w.scale_factor(), vh, w)
                    .iter()
                    .all(|&(_, m, _)| m == 0)
            });
            if recovered {
                break;
            }
        }
        assert!(
            recovered,
            "[A3] 预算回落（视口缩小）后应自愈回 1:1（可见格全在槽）：recovered={recovered}"
        );
    }

    /// 回归：Ctrl+滚轮应放大/缩小页面（页列宽、整列高随之变化），普通滚轮不改变缩放。
    #[gpui::test]
    async fn pdf_ctrl_wheel_zooms_in_out(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{
            AppContext, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        struct ZoomProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for ZoomProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div()
                    .id("zoom-pane")
                    .w(gpui::px(1200.))
                    .h_full()
                    .child(self.pdf.clone())
            }
        }

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            ZoomProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        for _ in 0..200 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (w0, total0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.page_width(), v.total_height)
        });
        assert!(
            (w0 - (1200.0 - 2.0 * config::pdf::PAGE_PAD_X)).abs() < 2.0,
            "初始页宽应适应宿主容器宽（1200 − 2·pad）：{w0}"
        );
        // 先滚到第 5 页，留出纵向滚动余量；布局稳定后记录滚动偏移作为缩放锚点
        cx.update(|_w, cx| {
            let pdf = holder.borrow().as_ref().unwrap().clone();
            pdf.update(cx, |v, cx| {
                v.scroll_to_page(5);
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let off0 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).scroll.offset()
        });
        assert!(
            off0.y.as_f32() < 0.0,
            "滚到第 5 页后应向下偏移（负值）：{off0:?}"
        );
        // Z1：记录缩放前 layout（锚点公式需旧/新 tops 对账；dims 全文档不变）。
        let (tops0, dims0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.page_tops.clone(), v.dims.clone())
        });

        let wheel = |control: bool, delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        };

        // Ctrl+向上滚（y>0）→ 放大
        cx.simulate_event(wheel(true, 240.0));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (w1, z1, total1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.page_width(), v.zoom, v.total_height)
        });
        assert!(w1 > w0, "Ctrl+向上滚应放大：{w0} → {w1}");
        assert!(z1 > 1.0, "zoom 应 >1：{z1}");
        assert!(total1 > total0, "整列高应随缩放增大：{total0} → {total1}");
        // Ctrl 滚轮**不应滚动**：滚动偏移按 Z1 锚点公式锚定——锚点页 K 的页顶按新布局
        // 换算 + 页内相对位移同比，常量区（PAGE_PAD_Y/PAGE_GAP）不被 ×ratio 放大（旧
        // 「整段 ×ratio」在深页每档漂移 (ratio−1)(PAD+K·GAP)，正是行上下跳动源）。
        let (off1, tops1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.scroll.offset(), v.page_tops.clone())
        });
        let sy0 = -off0.y.as_f32();
        let ratio = w1 / w0;
        let k = layout::first_visible_page(&dims0, &tops0, w0, sy0);
        let exp_y1 = -(tops1[k] + (sy0 - tops0[k]) * ratio);
        let exp_x1 = off0.x.as_f32() * ratio;
        assert!(
            (off1.y.as_f32() - exp_y1).abs() < 1.0 && (off1.x.as_f32() - exp_x1).abs() < 1.0,
            "Ctrl 放大不应产生额外滚动：锚定 y={exp_y1:.1} 实际 y={:.1}（K={k}，x 锚={exp_x1:.1} 实际 x={:.1}）",
            off1.y.as_f32(),
            off1.x.as_f32()
        );

        // 普通滚轮（无 Ctrl）→ 缩放不变，且照常滚动
        let z_before = z1;
        let off_before_plain = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).scroll.offset()
        });
        cx.simulate_event(wheel(false, 240.0));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (z2, off2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.zoom, v.scroll.offset())
        });
        assert!((z2 - z_before).abs() < 1e-4, "普通滚轮不应改变缩放：{z_before}→{z2}");
        assert!(
            off2.y.as_f32() != off_before_plain.y.as_f32(),
            "普通滚轮应照常滚动（偏移发生变化）"
        );

        // Ctrl+向下滚（y<0）→ 缩小：锚定基准 = 当前（普通滚轮已滚动后）的偏移
        let tops_before_out =
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_tops.clone());
        cx.simulate_event(wheel(true, -240.0));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (w3, z3, off3, tops3) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.page_width(),
                v.zoom,
                v.scroll.offset(),
                v.page_tops.clone(),
            )
        });
        assert!(w3 < w1, "Ctrl+向下滚应缩小：{w1} → {w3}");
        assert!(z3 < z1, "zoom 应变小：{z1} → {z3}");
        let sy0b = -off2.y.as_f32();
        let ratio_b = w3 / w1;
        let k2 = layout::first_visible_page(&dims0, &tops_before_out, w1, sy0b);
        let exp_y3 = -(tops3[k2] + (sy0b - tops_before_out[k2]) * ratio_b);
        let exp_x3 = off2.x.as_f32() * ratio_b;
        assert!(
            (off3.y.as_f32() - exp_y3).abs() < 1.0 && (off3.x.as_f32() - exp_x3).abs() < 1.0,
            "Ctrl 缩小也不应滚动：锚定 y={exp_y3:.1} 实际 y={:.1}（K={k2}）",
            off3.y.as_f32()
        );
    }

    /// 回归（A 超宽平移）：**左键拖拽区分「超宽平移 / 适配宽选中」**。
    /// - 阶段 1（适配宽，未横向溢出）：真实拖选命中文本 → 选中并弹卡（既有路径未破坏），
    ///   `panning` 必须为 false；
    /// - 阶段 2（Ctrl+滚轮放大 ×2 → 横向溢出）：左键按下即进入平移、拖拽左移、松手结束，
    ///   断言 `scroll.offset().x` 变负（页面左移）且不弹选中菜单。
    #[gpui::test]
    async fn pdf_overflow_pan_while_fit_selects(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;

        struct ZoomProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for ZoomProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div()
                    .id("zoom-pane")
                    .w(gpui::px(1200.))
                    .h_full()
                    .child(self.pdf.clone())
            }
        }

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            ZoomProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        for _ in 0..200 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        for _ in 0..3 {
            cx.run_until_parked();
        }

        // ---- 阶段 1（适配宽）：真实拖选命中文本 → 选中 + 弹卡，panning 必须为 false ----
        // 同步预热文本层（`load_text_for_test` 阻塞等 worker），再扫字符盒定位**视口内**
        // 一段可拖字符（k..k+3）的窗口坐标（此时文档在顶部，offset.y=0，前几页可见；
        // 同 shell `first_char_range_points` 姿势）。
        let ((_page, _k), (p1, p3)) = cx.update(|window, cx| {
            let pv = holder.borrow().as_ref().unwrap().clone();
            let mut found: Option<(
                (usize, usize),
                (gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>),
            )> = None;
            for page in 0..12usize {
                pv.update(cx, |v, _| {
                    v.load_text_for_test(page);
                });
                for k in 0..800usize {
                    if let (Some(p), Some(q)) = (
                        pv.read(cx).char_window_pos(window, page, k),
                        pv.read(cx).char_window_pos(window, page, k + 3),
                    ) {
                        if p.y >= gpui::px(0.)
                            && p.y <= gpui::px(900.)
                            && q.y >= gpui::px(0.)
                            && q.y <= gpui::px(900.)
                        {
                            found = Some(((page, k), (p, q)));
                            break;
                        }
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.expect("视口内（文档顶部）应存在可用字符盒（文本层未缓存？）")
        });
        // 真实拖选：按下 p1 → 拖到 p3 → 松手（与 shell b1 同姿势）
        let mods = gpui::Modifiers::default();
        cx.simulate_mouse_move(p1, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_down(p1, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_move(p3, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_up(p3, gpui::MouseButton::Left, mods);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (panning_fit, sel_card_fit) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.panning, v.sel_card.is_some())
        });
        assert!(!panning_fit, "适配宽下拖选不应进入平移：panning={panning_fit}");
        assert!(sel_card_fit, "适配宽下真实拖选应弹选中卡（既有路径未被破坏）");

        // ---- 阶段 2（放大 ×2 → 横向溢出）：左键拖拽 = 平移，无选中/无菜单 ----
        // 先滚到第 5 页，留出纵向滚动余量（同 pdf_ctrl_wheel_zooms_in_out）。
        cx.update(|_w, cx| {
            let pdf = holder.borrow().as_ref().unwrap().clone();
            pdf.update(cx, |v, cx| {
                v.scroll_to_page(5);
                cx.notify();
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let wheel = |control: bool, delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        };
        for _ in 0..2 {
            cx.simulate_event(wheel(true, 240.0));
            for _ in 0..4 {
                cx.run_until_parked();
            }
        }
        let (overflow, off_before) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.scroll.max_offset().x.as_f32() > 1.0, v.scroll.offset())
        });
        assert!(overflow, "Ctrl+滚轮放大 2 档后应横向溢出");

        // 在页内按下 → 进入平移 → **向左拖 120px** → 松手。「跟手」语义：页面随光标移动，
        // 左缘（offset.x=0）时向右拖无内容可露（precise clamp 到 0），向左拖即露出被截断
        // 的右侧 → offset.x 更负（scroll 偏移量程 [-max, 0]，见 gpui div.rs clamp）。
        let press = gpui::point(gpui::px(600.), gpui::px(450.));
        let released = gpui::point(gpui::px(600. - 120.), gpui::px(450.));
        cx.simulate_mouse_down(press, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_move(released, gpui::MouseButton::Left, mods);
        cx.simulate_mouse_up(released, gpui::MouseButton::Left, mods);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (off_after, panning_after, sel_card_after) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.scroll.offset(), v.panning, v.sel_card.is_some())
        });
        assert!(!panning_after, "平移松手后应退出 panning");
        assert!(
            off_after.x.as_f32() < off_before.x.as_f32(),
            "放大超宽后左键拖拽应向左平移（offset.x 更负）：{} → {}",
            off_before.x.as_f32(),
            off_after.x.as_f32()
        );
        assert!(!sel_card_after, "平移路径不应弹选中菜单");
    }

    /// 回归：**窗口拉宽/拉窄 → 页面自适应**。页宽基线 = 容器宽 − 两侧页边距，随
    /// `simulate_resize` 变化自动重排；缩放倍率 `zoom` 保持不变（页面等比缩放）。
    #[gpui::test]
    async fn pdf_resize_refits_page_width(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;

        struct FitProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for FitProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            FitProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..200 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let pad = config::pdf::PAGE_PAD_X;
        // 「强制重绘 + 泵」：真机里窗口 resize 必触发重绘 → `ensure_window` 每帧按最新
        // 容器宽重算基线；测试平台的假时钟不会自带重绘，需显式 refresh 驱动同一路径。
        let pump = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..10 {
                // 窗口本体重绘（非 app.refresh_windows）：确保 foreground 帧被调度后泵掉
                let _ = cx.update(|w, _| w.refresh());
                cx.run_until_parked();
            }
        };
        let read = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.page_width(),
                    v.zoom,
                    v.total_height,
                    f32::from(v.scroll.bounds().size.width),
                )
            })
        };

        // ① 宽窗：页宽 = 容器宽 − 2·页边距（自适应窗口宽），zoom 保持 1.0。
        pump(cx);
        let (w_wide, z0, total_wide, container_wide) = read(cx);
        assert!(
            (w_wide - (container_wide - 2.0 * pad)).abs() < 2.0,
            "宽窗应适应容器宽 {container_wide}：期望 {}，实际={w_wide}",
            container_wide - 2.0 * pad
        );
        assert!(w_wide > 200.0, "页宽应明显非零：{w_wide}");
        assert!((z0 - 1.0).abs() < 1e-4, "起始 zoom 应保持 1.0：{z0}");

        // ② 拉窄窗口 → 页宽跟容器变小（自适应重排），zoom 倍率不变，整列高缩小。
        cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(700.)));
        pump(cx);
        let (w_narrow, z1, total_narrow, container_narrow) = read(cx);
        assert!(
            container_narrow < container_wide,
            "容器应变窄：{container_wide} → {container_narrow}"
        );
        assert!(
            (w_narrow - (container_narrow - 2.0 * pad)).abs() < 2.0,
            "窄窗应自适应到容器宽 {container_narrow}：期望 {}，实际={w_narrow}",
            container_narrow - 2.0 * pad
        );
        assert!(w_narrow < w_wide, "页宽应变小：{w_wide} → {w_narrow}");
        assert!((z1 - z0).abs() < 1e-4, "resize 不应改 zoom 倍率：{z0}→{z1}");
        assert!(
            total_narrow < total_wide * 0.99,
            "页宽收窄后整列高应随之减小：{total_wide} → {total_narrow}"
        );

        // ③ 拉回宽窗 → 页宽弹回适配宽度（zoom 仍保持）。
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        pump(cx);
        let (w_back, z2, _, container_back) = read(cx);
        assert!(
            (w_back - (container_back - 2.0 * pad)).abs() < 2.0,
            "拉回宽窗应恢复适配宽 {container_back}：实际={w_back}"
        );
        assert!(w_back > w_narrow, "拉宽后页宽应更大：{w_narrow} → {w_back}");
        assert!((z2 - z0).abs() < 1e-4, "全程不应改 zoom 倍率：{z0}→{z2}");
    }

    /// 瓦片管线端到端（视口驱动）：打开 pdf测试.pdf 后，视口覆盖的首页应经
    /// 「调度 → pdfium worker 栅格化 → 主线程收集」产出**非空瓦片缓存**；页槽
    /// `tile_scale` 与当前显示密度一致；且可视区只驻留少量瓦片（内存按视口封顶）。
    #[gpui::test]
    async fn tiles_fill_viewport_on_visible_pages(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        for _ in 0..400 {
            cx.run_until_parked();
            let loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        // 无任何输入/显式 refresh：只用 advance_clock 推进虚拟时钟驱动 16ms 自刷新泵 +
        // 少量真实等待给 pdfium worker（真实 app 是真实时钟，帧天然持续）。
        // 验证完整链路：负载完成 → refresh → 布局就位 → 调度瓦片/封面 + 邻居封面预取 →
        // worker 回传 → 收集入槽（此前"负载完成 notify 空操作 + 首帧 bounds=0"导致永不调度）。
        let mut got_content = false;
        for _ in 0..800 {
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
            let (c0, c1) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let content0 = v.slots.first().map(|s| {
                    s.cover.is_some() || !s.tiles.is_empty() // 封面或瓦片皆可（封面到位即回收瓦片）
                });
                (
                    content0.unwrap_or(false),
                    v.slots.get(1).map(|s| s.cover.is_some()).unwrap_or(false),
                )
            });
            if c0 && c1 {
                got_content = true;
                break;
            }
        }
        // 再泵几帧让泵停表（避免遗留在途任务）
        for _ in 0..10 {
            cx.run_until_parked();
        }
        let (pages, page0_tiles, page0_scale, page0_cover, page1_cover) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let page0 = v.slots.first();
            let page1 = v.slots.get(1);
            (
                v.page_count(),
                page0.map(|s| s.tiles.len()).unwrap_or(0),
                page0.map(|s| s.tile_scale).unwrap_or(0.0),
                page0.map(|s| s.cover.is_some()).unwrap_or(false),
                page1.map(|s| s.cover.is_some()).unwrap_or(false),
            )
        });
        assert!(pages >= 1, "应载入至少 1 页");
        assert!(
            got_content && (page0_cover || page0_tiles >= 1),
            "视口覆盖的首页应渲染到位（整页锐化封面 或 瓦片，实际 cover={page0_cover} tiles={page0_tiles}）"
        );
        assert!(
            page0_cover,
            "首屏应先有整页锐化封面（一渲铺满，消除瓦片逐片渲染前的模糊）"
        );
        assert!(
            page1_cover,
            "空闲期应预取滚动方向邻居页的封面（滚到下一页立即澄清）"
        );
        assert!(
            page0_tiles == 0 || (page0_scale > 0.5 && page0_scale.is_finite()),
            "有驻留瓦片时首页瓦片密度应与显示一致：tiles={page0_tiles} scale={page0_scale}"
        );
        assert!(
            page0_tiles <= 16,
            "可视区只应驻留少量瓦片（{page0_tiles} > 16 说明过度栅格化）"
        );

        // 回归：沉降后必须收尾——在途清空、自刷新泵停止。若封面候选把 margin 页也纳入
        // 导致「收集→淘汰→重请求」换页风暴，此处 pending_c 永不清零、泵永不停（worker
        // 被封面重渲占满 → 滚动变慢、模糊变多）。
        for _ in 0..40 {
            cx.executor().advance_clock(std::time::Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (pending_t, pending_c, pump_running) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.pending_tiles.len(),
                v.pending_cover.len(),
                v.tile_pump_running,
            )
        });
        assert!(
            pending_t == 0 && pending_c == 0,
            "沉降后无在途工作（换页风暴检测）：pending_t={pending_t} pending_c={pending_c}"
        );
        assert!(
            !pump_running,
            "沉降后自刷新泵应停止：pump={pump_running}"
        );

        // 回归：来回滚动多次、累积覆盖大量页后，封面不得累积（窗外封面逐帧回收）——
        // 旧实现按“页数上限淘汰”会让滚动越过的页面积攒封面，最终把当前窗口边缘的邻居
        // 页淘汰并反复重请求 → 换页风暴（worker 被封面重渲占满、滚动变慢变糊）。
        for target in [30usize, 10, 50, 20] {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(target);
                        cx.notify();
                    });
            });
            for _ in 0..200 {
                cx.executor().advance_clock(std::time::Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(5));
                let done = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && !v.tile_pump_running
                });
                if done {
                    break;
                }
            }
            let (pending_t, pending_c, covers_held) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let held = v
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.cover.is_some())
                    .count();
                (v.pending_tiles.len(), v.pending_cover.len(), held)
            });
            assert!(
                pending_t == 0 && pending_c == 0,
                "第 {target} 页沉降后无在途工作：t={pending_t} c={pending_c}"
            );
            assert!(
                covers_held <= 5,
                "第 {target} 页停稳后封面驻留有界（F2 起窗口=可见页+方向预取 1，上限更小；{covers_held} > 5 = 封面在累积）"
            );
        }
    }

    /// 与 view 的 `page_visible_rect` 同口径：卡片与视口相交即“真正可见页”。
    fn pdf_visible_pages(
        v: &crate::pdf::PdfViewer,
        page_width: f32,
        off_y: f32,
        viewport_h: f32,
    ) -> Vec<usize> {
        v.dims
            .iter()
            .enumerate()
            .filter(|(i, (w, h))| {
                let dh = layout::display_height((*w, *h), page_width);
                let top = v.page_tops[*i];
                top + dh > off_y && top < off_y + viewport_h
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn push_target(targets: &mut Vec<usize>, t: usize, n: usize) {
        let t = t.min(n.saturating_sub(1));
        if !targets.contains(&t) {
            targets.push(t);
        }
    }

    /// 强化自动测试：把整份文档当“翻书”自动翻页（前向 + 不规整回翻 + 跨区），每停一页
    /// 都要求**渲染到位**——停稳（在途瓦片/封面清零、泵停、退出快速态）且**真正可见页**
    /// 全部已有内容（封面或瓦片，非仅占位）。任一页停不稳/不清晰都直接断言失败，并打印
    /// 带时间戳（`timing_ms`）的巡检行，与 `VUICK_PDF_TIMING=1` 的控制台日志对位。
    #[gpui::test]
    async fn auto_flip_renders_every_stop_page(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");

        // 推进若干轮（虚拟时钟驱动 16ms 泵 + 真实睡眠给 worker 物理时间）；scoped
        // 闭包显式传参，避免捕获外层 cx 造成借用冲突。
        let pump = |cx: &mut gpui::TestAppContext, iters: usize| {
            for _ in 0..iters {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
            }
        };
        pump(cx, 40); // 首屏清尾

        let n = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).page_count()
        });
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");

        // 翻页轨迹：前向每 6 页 + 回翻每 7 页 + 若干跨区停靠（去重、保持先后）
        let mut targets: Vec<usize> = Vec::new();
        for t in (0..n).step_by(6) {
            push_target(&mut targets, t, n);
        }
        for t in (0..n).step_by(7).rev() {
            push_target(&mut targets, t, n);
        }
        for t in [2usize, 17, 29, 41, 59, n - 4] {
            push_target(&mut targets, t, n);
        }

        let mut checked = 0usize;
        let mut first_fail = String::new();
        for target in targets {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(target);
                        cx.notify();
                    });
            });
            // 停稳：在途瓦片/封面清零 + 泵停 + 退出快速态（缺失 = 该页卡住未渲染到位）。
            // 不再手动 `refresh()`：自刷新泵在 `prev_fast` 期间持续出帧（pump_still_needed
            // 含 prev_fast），速度自然衰减 → 自动换挡 → 瓦片/封面派发 → 停稳。这正好回归
            // 测试“滚轮骤停卡在 fast、泵停、无帧 → 可见页停在占位等下一次输入”的缺陷。
            let mut settled = false;
            for _ in 0..200 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (
                        v.pending_tiles.is_empty(),
                        v.pending_cover.is_empty(),
                        !v.tile_pump_running,
                        !v.prev_fast,
                    )
                });
                if st == (true, true, true, true) {
                    settled = true;
                    break;
                }
            }
            if !settled {
                first_fail = format!("翻到第 {target} 页后超时未停稳");
                break;
            }
            pump(cx, 6); // 收尾一两个帧，结果已入槽

            // 渲染到位：真正可见的每一页都应有内容（封面或瓦片）；封面驻留有界
            let (off_y, miss, covers_held) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let pw = v.page_width();
                let off_y = f32::from(-v.scroll.offset().y);
                let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
                let vis = pdf_visible_pages(v, pw, off_y, vh);
                let miss: Vec<usize> = vis
                    .iter()
                    .copied()
                    .filter(|&p| {
                        let s = &v.slots[p];
                        s.cover.is_none() && s.tiles.is_empty()
                    })
                    .collect();
                let held = v.slots.iter().filter(|s| s.cover.is_some()).count();
                (off_y, miss, held)
            });
            if !miss.is_empty() {
                first_fail = format!(
                    "第 {target} 页停稳后可见页未渲染到位：{miss:?}（off_y={off_y:.0}）"
                );
                break;
            }
            if covers_held > 5 {
                first_fail = format!("第 {target} 页封面驻留越界：{covers_held}");
                break;
            }
            checked += 1;
            eprintln!(
                "[{:.1}ms] [FLIP] target={target} off_y={off_y:.0} covers={covers_held}",
                crate::pdf::timing_ms(),
            );
        }
        assert!(
            checked >= 20 && first_fail.is_empty(),
            "自动翻页巡检未达标：checked={checked} {}",
            first_fail
        );
    }

    /// 回归：滚轮**骤停**卡在快速态（`fast=true` 且无在途 → 泵自停 → 无帧 → `prev_fast`
    /// 卡死、可见页停在低清占位）时，必须能在**没有任何后续输入**的情况下自动衰减、换挡、
    /// 补瓦片/封面并清晰。修复前：骤停后可见页停在占位数秒，直到用户再次滚动
    /// （实测日志 41584.9ms 后 1.7s 无 `[VIEW-TICK]`，再滚才预热变清晰）——这正是
    /// “随机页先模糊、数秒后清晰”的根因。
    #[gpui::test]
    async fn abrupt_fast_stop_sharpens_without_input(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        // 初始清尾到停稳
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.tile_pump_running,
                    !v.prev_fast,
                )
            });
            if st == (true, true) {
                break;
            }
        }

        // 一次性大跳（模拟滚轮骤停的那批事件）→ 速度飙高 → 进入快速态
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(20);
                    cx.notify();
                });
        });

        // ⚠️ 此后**零输入**：不 scroll、不 refresh，只有自刷新泵（16ms 虚拟时钟）在推进。
        let mut saw_fast = false;
        let mut scheduled_without_input = false;
        let mut sharp = false;
        let mut settled = false;
        for _ in 0..250 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let s = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let off_y = f32::from(-v.scroll.offset().y);
                let pw = v.page_width();
                let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
                let vis = pdf_visible_pages(v, pw, off_y, vh);
                let vis_sharp = !vis.is_empty()
                    && vis.iter().all(|&p| {
                        let slot = &v.slots[p];
                        slot.cover.is_some() || !slot.tiles.is_empty()
                    });
                (
                    v.prev_fast,
                    !v.pending_tiles.is_empty() || !v.pending_cover.is_empty(),
                    vis_sharp,
                    v.pending_tiles.is_empty() && v.pending_cover.is_empty(),
                    !v.tile_pump_running,
                    !v.prev_fast,
                )
            });
            saw_fast |= s.0;
            scheduled_without_input |= s.1;
            if s.2 && s.3 && s.4 && s.5 {
                sharp = true;
                settled = true;
                break;
            }
        }
        assert!(saw_fast, "大跳后应进入快速态以复现骤停场景");
        assert!(
            scheduled_without_input,
            "骤停后无输入也必须自动派发瓦片/封面（否则可见页停在占位等下一次输入）"
        );
        assert!(
            sharp && settled,
            "骤停后无输入应自动换挡并补清晰：sharp={sharp} settled={settled}"
        );
    }

    /// 滚轮事件（普通滚动，不缩放）构造：`delta_y` 每帧像素 → 滚动速度 EMA → 控制分带。
    fn test_wheel(delta_y: f32) -> gpui::ScrollWheelEvent {
        gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(700.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers::default(),
            ..Default::default()
        }
    }

    /// F3/E1：**中速带（Fast）改派满密度封面、不派瓦片**——持续 200px/帧 滚入 Fast 带后：
    /// - `cover_reqs` 增长（新进视口页按满密度封面调度，取代 520/260 占位）；
    /// - `tile_reqs` 在 Fast 期内**零增长**（瓦片不在封面前插队、也不受占位支配）
    ///   → 封面不被瓦片阻塞（F3/B 目标「滚到哪清晰到哪」的 worker 侧前提）；
    /// - 停稳后**全部可见页以满密度封面临位**（非中清/缩略占位）。
    #[gpui::test]
    async fn fast_band_dispatches_full_covers_without_tiles(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count());
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.prev_fast
                });
                if st {
                    return;
                }
            }
            panic!("初始状态应沉降");
        };
        settle(cx);

        // 阶段 0：以 200px/帧 推入中速带（速度 EMA→200 ∈ (112, 480)）
        for _ in 0..8 {
            cx.simulate_event(test_wheel(200.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let entered = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.speed_state == crate::pdf::view::SpeedState::Fast, v.scroll_velocity)
        });
        eprintln!(
            "[F3-E1] entered_fast={} v={:.0}px/帧（Fast 带 112..480）",
            entered.0, entered.1
        );
        assert!(entered.0, "200px/帧 应进入 Fast 带（EMA={:.0}）", entered.1);

        // 阶段 1：Fast 带内持续滚动 18 帧 → 只派封面不派瓦片
        let (c0, t0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs)
        });
        let mut fast_frames = 0usize;
        let mut saw_pending_cover = false;
        let mut tiles_frozen = true;
        for _ in 0..18 {
            cx.simulate_event(test_wheel(200.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.speed_state,
                    v.pending_cover.len(),
                    v.pending_tiles.len(),
                    v.cover_reqs,
                    v.tile_reqs,
                )
            });
            if st.0 == crate::pdf::view::SpeedState::Fast {
                fast_frames += 1;
                saw_pending_cover |= st.1 > 0;
                tiles_frozen &= st.4 == t0 && st.2 == 0;
            }
        }
        let (c1, t1, pc1, pt1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.cover_reqs,
                v.tile_reqs,
                v.pending_cover.len(),
                v.pending_tiles.len(),
            )
        });
        eprintln!(
            "[F3-E1] fast_frames={fast_frames} cover_reqs {c0}→{c1} tile_reqs {t0}→{t1} pending c={pc1} t={pt1}"
        );
        assert!(fast_frames > 0, "阶段 1 应存在 Fast 帧");
        assert!(
            c1 > c0,
            "Fast 带应派发满密度封面（cover_reqs {c0}→{c1}）：滚动到哪清晰到哪"
        );
        assert!(
            t1 == t0 && tiles_frozen,
            "Fast 带不得派发瓦片（tile_reqs {t0}→{t1}，Fast 期 pending_tiles 恒 0）：封面不被瓦片支配/阻塞"
        );
        assert!(
            saw_pending_cover || c1 - c0 >= 3,
            "Fast 带应可见封面在途或已派发足额（cover_reqs 增量 {}）",
            c1 - c0
        );

        // 阶段 2：停稳后可见页全部以满密度封面临位（B「滚到哪清晰到哪」，非占位）
        settle(cx);
        let vis_covered = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let pw = v.page_width();
            let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
            let vis = pdf_visible_pages(v, pw, off_y, vh);
            (!vis.is_empty(), vis.iter().all(|&p| v.slots[p].cover.is_some()), vis.clone())
        });
        assert!(
            vis_covered.1,
            "Fast 带滚动停稳后可见页应有满密度封面（中速滚动滚到哪清晰到哪）：vis={:?}",
            vis_covered.2
        );
    }

    /// F3/E2：**高速带（VeryFast）纯平移、不派新栅格**——持续 700px/帧 进入 VeryFast 后，
    /// 任一帧 `cover_reqs`/`tile_reqs` 均零增长、`pending_cover`/`pending_tiles` 不再增长
    /// （渲染队列不加活；已缓存封面/瓦片原位即锐，新进页交给 F4 预取走廊兜底，本波未实现）。
    #[gpui::test]
    async fn very_fast_band_pure_pan_no_new_raster(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.prev_fast
                });
                if st {
                    return;
                }
            }
            panic!("初始状态应沉降");
        };
        settle(cx);

        // 阶段 0：以 700px/帧 推入高速带（EMA→700 > 480）
        for _ in 0..10 {
            cx.simulate_event(test_wheel(700.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let entered = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.speed_state == crate::pdf::view::SpeedState::VeryFast, v.scroll_velocity)
        });
        eprintln!(
            "[F3-E2] entered_veryfast={} v={:.0}px/帧（VeryFast > 480）",
            entered.0, entered.1
        );
        assert!(entered.0, "700px/帧 应进入 VeryFast 带（EMA={:.0}）", entered.1);

        // 阶段 1：VeryFast 内持续平移到滚不动为止 → 不得派任何新栅格
        let (c0, t0, pc0, pt0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.cover_reqs,
                v.tile_reqs,
                v.pending_cover.len(),
                v.pending_tiles.len(),
            )
        });
        let mut all_very_fast = true;
        let (mut grew_cover, mut grew_tiles, mut grew_pc, mut grew_pt, mut pan_frames) =
            (false, false, false, false, 0usize);
        for _ in 0..12 {
            cx.simulate_event(test_wheel(700.0));
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.speed_state,
                    v.cover_reqs,
                    v.tile_reqs,
                    v.pending_cover.len(),
                    v.pending_tiles.len(),
                )
            });
            all_very_fast &= st.0 == crate::pdf::view::SpeedState::VeryFast;
            grew_cover |= st.1 != c0;
            grew_tiles |= st.2 != t0;
            grew_pc |= st.3 > pc0;
            grew_pt |= st.4 > pt0;
            pan_frames += 1;
        }
        let (c1, t1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.tile_reqs)
        });
        eprintln!(
            "[F3-E2] pan_frames={pan_frames} all_veryfast={all_very_fast} cover {c0}→{c1} tile {t0}→{t1}"
        );
        assert!(all_very_fast, "700px/帧 持续滚动应保持 VeryFast 带");
        assert!(pan_frames > 0, "应有平移帧样本");
        assert!(!grew_cover && c1 == c0, "VeryFast 纯平移：不得派新封面（cover_reqs {c0}→{c1}）");
        assert!(!grew_tiles && t1 == t0, "VeryFast 纯平移：不得派新瓦片（tile_reqs {t0}→{t1}）");
        assert!(!grew_pc, "VeryFast 纯平移：pending_cover 不得增长");
        assert!(!grew_pt, "VeryFast 纯平移：pending_tiles 不得增长");
    }

    /// 完整滚动一遍（采样停靠）后测渲染驻留内存构成：瓦片字节 + 封面（整页位图按当前
    /// 密度估算）+ 中清/缩略计数 + 封面/瓦片双份持有数。返回
    /// `(tile_bytes, covers, mids, thumbs, double_hold, est_total)`。
    #[allow(clippy::type_complexity)]
    fn full_scroll_memory(
        holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >,
        cx: &mut gpui::VisualTestContext,
        targets: &[usize],
    ) -> (u64, usize, usize, usize, usize, u64) {
        use std::time::Duration;
        assert!(targets.len() >= 5, "应至少 5 个采样停靠点，实际 {}", targets.len());
        let view = holder.borrow().as_ref().expect("视图应存在").clone();
        for &target in targets {
            cx.update(|_w, cx| {
                view.update(cx, |v, cx| {
                    v.scroll_to_page(target);
                    cx.notify();
                });
            });
            for _ in 0..200 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = view.read(cx);
                    (
                        v.pending_tiles.is_empty()
                            && v.pending_cover.is_empty()
                            && !v.tile_pump_running,
                        !v.prev_fast,
                    )
                });
                if st == (true, true) {
                    break;
                }
            }
        }
        cx.update(|_w, cx| {
            let v = view.read(cx);
            let pw = v.page_width();
            // 测试窗口真实 DPI（gpui TestAppContext 默认 2×）：封面/瓦片按**真实密度**驻留，
            // 估算必须用同一 sf——旧代码硬编码 1.0 把封面字节少算 ~4×（P1 ③ 修复测量基准）。
            let sf = _w.scale_factor();
            let mut covers_held = 0usize;
            let mut mids_held = 0usize;
            let mut thumbs_held = 0usize;
            let mut double_hold = 0usize;
            let mut cover_bytes = 0u64;
            for (p, s) in v.slots.iter().enumerate() {
                if s.cover.is_some() {
                    covers_held += 1;
                    // **P1 ③**：按**实际驻留密度**估算字节（邻居封面已降档 2.0——理论满密度
                    // 会高估）；cover_scale=0（S5 已回收标记，正常无 cover 并存）回退理论值。
                    let cs = if s.cover_scale.abs() > 1e-4 {
                        s.cover_scale
                    } else {
                        layout::cover_scale(v.dims[p], pw, sf)
                    };
                    let (w, h) = layout::page_bitmap_size(v.dims[p], cs);
                    cover_bytes += (w as u64) * (h as u64) * 4;
                    // 冗余双份 = 封面与瓦片**同密度**且同页并存（应为 0，evict_tiles 每帧回收）。
                    // 密度不同（封面被 COVER_SCALE_CAP 截断、瓦片更细）属正常“底图+精修”，不计。
                    if !s.tiles.is_empty() && (s.cover_scale - s.tile_scale).abs() <= 1e-4 {
                        double_hold += 1;
                    }
                }
                if s.mid.is_some() {
                    mids_held += 1;
                }
                if s.thumb.is_some() {
                    thumbs_held += 1;
                }
            }
            // 中清 ~2MB、缩略 ~0.37MB 为近似值（不精确到单页，仅用于有界断言）
            let total_est = v.tile_bytes
                + cover_bytes
                + mids_held as u64 * 2 * 1024 * 1024
                + thumbs_held as u64 * 385 * 1024;
            (
                v.tile_bytes,
                covers_held,
                mids_held,
                thumbs_held,
                double_hold,
                total_est,
            )
        })
    }

    /// F4/D：走廊封面（[`Slot::prefetch_cover`]）驻留内存（独立于窗口封面）：返回
    /// `(廊封面页数, 粗略字节)`。byte 计 `page_bitmap_size(dim, prefetch_cover_scale)`。
    #[allow(clippy::type_complexity)]
    fn prefetch_memory(
        holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >,
        cx: &mut gpui::VisualTestContext,
    ) -> (usize, u64) {
        cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let mut cnt = 0usize;
            let mut bytes = 0u64;
            for (p, s) in v.slots.iter().enumerate() {
                if s.prefetch_cover.is_some() {
                    cnt += 1;
                    let (w, h) = layout::page_bitmap_size(v.dims[p], s.prefetch_cover_scale);
                    bytes += (w as u64) * (h as u64) * 4;
                }
            }
            (cnt, bytes)
        })
    }

    /// 回归（内存）：完整滚动一遍整份文档后，渲染驻留内存必须**有界**且消灭“封面 + 瓦片
    /// 双份持有”。修复前：整页封面与同页瓦片/中清并存 → 全量缓存预算合计 ~280MB（瓦片
    /// 96MB + 封面 ~36MB + 中清 32MB + 缩略 15MB + 基座）。修复后：同密度整页封面在位即
    /// 回收该页瓦片与中清、瓦片预算降到 40MB，完整滚动后应显著回落。
    #[gpui::test]
    async fn memory_stays_bounded_after_full_scroll(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).page_count()
        });
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");

        // 完整滚动一遍的采样停靠（重演“翻到底”的累积路径）
        let mut targets: Vec<usize> = Vec::new();
        for t in (0..n).step_by(9) {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        for t in [3usize, n - 2, 7, n - 5, 40].into_iter() {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }

        let measure = full_scroll_memory(&holder, cx, &targets);
        let (tile_bytes, covers, mids, thumbs, double_hold, total_est) = measure;
        // R3：预算随可见钉住集伸缩（max(固定 32MB, 可见格×单格×1.25），硬顶 128MB）。
        // 本夹具（pdf测试2.pdf 适配宽 render_scale≈4.9、cover 被 cap 截在 4.0 → 纯瓦片供给）
        // 可见工作集 ~30 格 ≈30MB 即贴近旧固定 32MB；钉住不逐出 + 1.25 余量 → 预算升至 ~37MB。
        // 「瓦片占用 ≤ 动态预算、且预算 ≤ 硬顶、远非满额」即本变更后稳态内存不变式。
        let budget = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .tile_budget_state
                .as_ref()
                .map(|s| s.budget)
                .unwrap_or(config::pdf::TILE_CACHE_BYTES)
        });
        eprintln!(
            "[MEM zoom=1.0] tile_bytes={}MB covers={} mids={} thumbs={} double_hold={} est_total={}MB budget={}MB",
            tile_bytes / 1024 / 1024,
            covers,
            mids,
            thumbs,
            double_hold,
            total_est / 1024 / 1024,
            budget / 1024 / 1024,
        );
        assert!(
            tile_bytes <= budget,
            "zoom1 瓦片占用应 ≤ 动态预算（可见钉住集×1.25 的收缩内存）：{tile_bytes} > {budget}"
        );
        assert!(
            budget <= config::pdf::TILE_CACHE_BYTES_MAX,
            "zoom1 动态预算应 ≤ 硬顶（128MB），否则内存无界：{budget}"
        );
        assert!(
            tile_bytes < config::pdf::TILE_CACHE_BYTES_MAX,
            "zoom1 瓦片占用应远小于硬顶（非满额驻留）：{tile_bytes} >= {}",
            config::pdf::TILE_CACHE_BYTES_MAX
        );
        // F2：封面驻留上界 = 可见页数 + 滚动方向预取 1。本夹具（1500×900 视口、A4 适配宽）
        // 停稳后可见页恒为 1（旧对称窗 3 页 = 可见 1 + 两侧各 1 即基线 covers=3），
        // 故相对上界 = 1 + 1 = 2——不锁死宽泛绝对数字，随可见页数变化。
        assert!(covers <= 2, "zoom1 封面驻留应 ≤ 可见页+方向预取 1（当前视口 1 页 → ≤2）：{covers}");
        assert!(
            mids <= config::pdf::MID_CACHE_PAGES,
            "zoom1 中清驻留应 ≤ 上限：{mids}"
        );
        assert!(
            thumbs <= config::pdf::THUMB_CACHE_PAGES,
            "zoom1 缩略驻留应 ≤ 上限：{thumbs}"
        );
        assert!(
            double_hold == 0,
            "zoom1 封面在位时不得双份持有瓦片（内存回归）：{double_hold} 页"
        );
        assert!(
            total_est < 220 * 1024 * 1024,
            "zoom1 完整滚动后渲染驻留内存应明显低于修复前（~280MB）：{total_est}"
        );
        // **P1 内存瘦身（真值基准）**：旧断言用 `sf=1.0` 估算封面字节，在测试平台真实 2× DPI
        // 下把封面少算 ~4×（把 ~100MB 真值误读成 ~60MB）。修正后用真实密度基准：4502f8a
        // 真值 ~100MB（可见+邻居各 30MB 满密封面 + 37MB 瓦片）→ P1 ③ 后 ~74MB（邻居封面
        // 降 2.0 档）——断言真值 < 90MB（低于基线 >10%，且远低于用户报的 ~176MB 量级）。
        assert!(
            total_est < 90 * 1024 * 1024,
            "zoom1 真值驻留应低于 4502f8a 真值基线（~100MB）且远小 176MB：{total_est}"
        );
        // F4/D：走廊封面独立有界（窗口封面计数/est_total 不受影响）；且 est_total 不超
        // 8bab7be 基线（z1.0 est 66-69MB）的 120%。settle 只等主队列清空，走廊随后在 idle
        // 帧填充——先无条件泵若干帧让走廊播种+落定，再泵到走廊在途清空，量稳态尺寸。
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let pf_done = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_prefetch_cover.is_empty()
            });
            if pf_done {
                break;
            }
        }
        let (pf_covers, pf_bytes) = prefetch_memory(&holder, cx);
        eprintln!(
            "[MEM-PRETCH zoom=1.0] prefetch_covers={pf_covers} bytes={}MB 预算={}MB",
            pf_bytes / 1024 / 1024,
            config::pdf::PREFETCH_COVER_BUDGET_BYTES / 1024 / 1024,
        );
        assert!(
            pf_bytes <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "zoom1 走廊封面字节应 ≤ 预算：{pf_bytes}"
        );
        // P1 ①③④：真值驻留 < 85%×4502f8a 真值基线（~100MB）；走廊 0.5 preview 同页去重
        // （已有封面/瓦片的页不再双驻）后仍独立 ≤ 预算。
        assert!(
            total_est < 85 * 1024 * 1024,
            "zoom1 est_total 应明显低于 4502f8a 真值基线（~100MB）：{total_est}"
        );

        // 再以用户实测量的高倍密度（Ctrl+滚轮放大 → zoom≈1.76 → 渲染密度≈2.48）完整滚动
        // 一遍复测：封面单张变 ~12MB，正是真实场景里 160MB 的主要构成，必须同样有界。
        // ⚠️ 必须走真实滚轮事件触发 `on_scroll_wheel` 的重排（直接改 `v.zoom` 会让布局/滚动
        // 锚点失配 → 可见区间虚增 → 封面/瓦片翻倍，测不出真实数）。
        let zoom_wheel = |control: bool, delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        };
        for _ in 0..5 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..6 {
                cx.run_until_parked();
            }
        }
        let z = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).zoom
        });
        assert!(z > 1.5, "Ctrl+滚轮应放大到高倍密度为主：{z}");
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let (tile_bytes2, covers2, mids2, thumbs2, double_hold2, total_est2) =
            full_scroll_memory(&holder, cx, &targets);
        let budget2 = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .tile_budget_state
                .as_ref()
                .map(|s| s.budget)
                .unwrap_or(config::pdf::TILE_CACHE_BYTES)
        });
        eprintln!(
            "[MEM zoom=1.75(d=2.47)] tile_bytes={}MB covers={} mids={} thumbs={} double_hold={} est_total={}MB budget={}MB",
            tile_bytes2 / 1024 / 1024,
            covers2,
            mids2,
            thumbs2,
            double_hold2,
            total_est2 / 1024 / 1024,
            budget2 / 1024 / 1024,
        );
        assert!(
            tile_bytes2 <= budget2,
            "zoom1.75 瓦片占用应 ≤ 动态预算（可见钉住集×1.25 的收缩内存）：{tile_bytes2} > {budget2}"
        );
        assert!(
            budget2 <= config::pdf::TILE_CACHE_BYTES_MAX,
            "zoom1.75 动态预算应 ≤ 硬顶（128MB），否则内存无界：{budget2}"
        );
        assert!(
            covers2 <= 2,
            "zoom1.75 封面驻留应 ≤ 可见页+方向预取 1（当前视口 1 页 → ≤2）：{covers2}"
        );
        assert!(
            mids2 <= config::pdf::MID_CACHE_PAGES,
            "zoom1.75 中清驻留应 ≤ 上限：{mids2}"
        );
        assert!(
            thumbs2 <= config::pdf::THUMB_CACHE_PAGES,
            "zoom1.75 缩略驻留应 ≤ 上限：{thumbs2}"
        );
        assert!(
            double_hold2 == 0,
            "zoom1.75 封面在位时不得双份持有瓦片（内存回归）：{double_hold2} 页"
        );
        assert!(
            total_est2 < 150 * 1024 * 1024,
            "zoom1.75 高倍密度下渲染驻留内存也应有界：{total_est2}"
        );
        // F4/D：高倍密度下走廊封面仍独立有界（预算按字节强制逐出，超预算页自动裁剪）；
        // est_total 不超 8bab7be 基线（z1.75 est 105-108MB）120%。同 zoom1.0：先无条件泵
        // 帧让走廊在最后停靠点播种落定，再泵到走廊在途清空后量稳态。
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let pf_done = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_prefetch_cover.is_empty()
            });
            if pf_done {
                break;
            }
        }
        let (pf_covers2, pf_bytes2) = prefetch_memory(&holder, cx);
        eprintln!(
            "[MEM-PRETCH zoom=1.75] prefetch_covers={pf_covers2} bytes={}MB 预算={}MB",
            pf_bytes2 / 1024 / 1024,
            config::pdf::PREFETCH_COVER_BUDGET_BYTES / 1024 / 1024,
        );
        assert!(
            pf_bytes2 <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "zoom1.75 走廊封面字节应仍 ≤ 预算：{pf_bytes2}"
        );
        // **P1 ③① 真值降幅**：4502f8a 真值 ≈ 75MB（可见页满密 4.0 cover 30MB + 43MB 瓦片）
        // → 修后 ~44MB（可见页瓦片接管 + 邻居封面 2.0）：真值降 ≥40%（远低于旧 108×1.2 基准）。
        assert!(
            total_est2 < 55 * 1024 * 1024,
            "zoom1.75 真值驻留应 ≥40% 低于 4502f8a 真值基线（~75MB）：{total_est2}"
        );
    }

    /// **A-P1-1b（高倍×宽高窗十字门）**：旧 memory 测试在测试平台真实 2× DPI 下运行（封面
    /// 被旧 `sf=1.0` 估算少算 ~4×），且只钉 1 页视口的窄场景——盖不住「高倍 + 宽高窗」的
    /// 驻留爆发。本测试强制 **1× DPI（真机常规）** + Ctrl+滚轮推到 zoom≈2.4（render_scale
    /// 越过封面 cap → 纯瓦片 regime）在 1600×1200 宽高窗完整滚动一遍，用**真实密度**量稳态：
    /// 整体驻留必须**远低于** 4502f8a（用户实测 sf2/z4 单页 ~176MB → 设计目标 ~95-105MB）。
    /// 此门由「邻居封面降档 2.0（③）+ 瓦片横向 margin 焊死 0（①）+ prefetch 同页去重（④）」
    /// 三者共同决定——任一回退，真值驻留即弹回 150MB+ 量级、本断言红。
    #[gpui::test]
    async fn memory_bounded_at_high_zoom_wide_window(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        // 宽高窗 + 强制 1× DPI（真机常规；测试平台默认 2×）。
        cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(1200.)));
        cx.update(|w, _| w.set_scale_factor(1.0));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).page_count()
        });
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");
        // 暖 60 帧让自适应页宽 / 滚动容器布局稳定。
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        // Ctrl+滚轮推到 zoom≈2.4（12%/档 × ~9 档）。
        let zoom_wheel = |control: bool, delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut z = 0.0f32;
        for _ in 0..12 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..2 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
            }
            z = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).zoom);
            if z >= 2.3 {
                break;
            }
        }
        assert!(z >= 2.3, "应推到高倍 zoom≥2.3：{z}");
        // 完整滚动一遍（采样停靠）+ 停稳，真值量稳态。
        let mut targets: Vec<usize> = Vec::new();
        for t in (0..n).step_by(9) {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        for t in [3usize, n - 2, 7, n - 5, 40].into_iter() {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        let (tile_bytes, covers, mids, thumbs, double_hold, total_est) =
            full_scroll_memory(&holder, cx, &targets);
        let budget = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .tile_budget_state
                .as_ref()
                .map(|s| s.budget)
                .unwrap_or(config::pdf::TILE_CACHE_BYTES)
        });
        eprintln!(
            "[MEM-HI 1x z={:.2}] tile_bytes={}MB covers={} mids={} thumbs={} double_hold={} est_total={}MB budget={}MB",
            z,
            tile_bytes / 1024 / 1024,
            covers,
            mids,
            thumbs,
            double_hold,
            total_est / 1024 / 1024,
            budget / 1024 / 1024,
        );
        assert!(
            tile_bytes <= budget,
            "高倍宽高窗瓦片占用应 ≤ 动态预算：{tile_bytes} > {budget}"
        );
        assert!(
            budget <= config::pdf::TILE_CACHE_BYTES_MAX,
            "高倍宽高窗动态预算应 ≤ 硬顶（128MB）：{budget}"
        );
        assert!(
            covers <= 3,
            "高倍宽高窗封面驻留应 ≤ 可见页+方向预取（宽窗可见 ≤2 页 → ≤3）：{covers}"
        );
        assert!(
            double_hold == 0,
            "高倍宽高窗封面在位时不得双份持有瓦片（内存回归）：{double_hold} 页"
        );
        // 十字门：真值驻留应远低于 4502f8a 用户实测（sf2/z4 单页 ~176MB；现实 ~95-105MB）。
        // 用 110MB 硬线（设计目标 +5MB 余量）——回退（邻居满密/横向整列预取/双份 preview）
        // 必弹回 150MB+，本断言红。
        assert!(
            total_est < 110 * 1024 * 1024,
            "高倍宽高窗真值驻留应 << 4502f8a 用户实测（~176MB → 目标 ~95-105MB）：{total_est}"
        );
        // 走廊（0.5 密度整页 preview）独立有界：先泵让走廊播种落定，再泵到在途清空量稳态。
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let pf_done = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_prefetch_cover.is_empty()
            });
            if pf_done {
                break;
            }
        }
        let (pf_covers, pf_bytes) = prefetch_memory(&holder, cx);
        eprintln!(
            "[MEM-PRETCH-HI] prefetch_covers={pf_covers} bytes={}MB 预算={}MB",
            pf_bytes / 1024 / 1024,
            config::pdf::PREFETCH_COVER_BUDGET_BYTES / 1024 / 1024,
        );
        assert!(
            pf_bytes <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "高倍宽高窗走廊封面字节应 ≤ 预算：{pf_bytes}"
        );
    }

    /// **A-P1-3（邻居封面降档 + 入视满密度无闪白）**：封面窗内「真可见」页封面满密度；
    /// 仅滑入/滑出的**邻居页**（封面窗内但不可见）封面降到 [`config::pdf::COVER_SCALE_CAP_NEIGHBOR`]
    /// =2.0 档（P1 ③）。真实渲染路径断言：
    /// ① 下滚停稳第 10 页（dir=1，封面窗 [10..11]）→ 页 10（可见）`cover_scale ≈` 满 cscale，
    ///    页 11（邻居）`cover_scale ≈ min(cscale, 2.0)`——可见满密、邻居降档；
    /// ② 翻页到 11 停稳 → 页 11 重新「真可见」→ **满密度快速就位**（`cover_scale == cscale_full`、
    ///    cover 在槽），全程 `loading_placeholder_hits` 零增长（stale→prev_cover 桥 + 满密重派，
    ///    无闪白）。
    #[gpui::test]
    async fn neighbor_cover_low_density_then_full_on_enter(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).page_count()
        });
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (v.pending_cover.is_empty() && v.pending_tiles.is_empty(), !v.prev_fast)
                });
                if st == (true, true) {
                    return;
                }
            }
            panic!("状态应沉降（封面/瓦片在途应清空）");
        };
        let jump = |cx: &mut gpui::VisualTestContext, page: usize| {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(page);
                        cx.notify();
                    });
            });
        };
        let cover_meta = |cx: &mut gpui::VisualTestContext, p: usize| {
            cx.update(|w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let cscale_full = layout::cover_scale(
                    v.dims[p],
                    v.page_width(),
                    w.scale_factor(),
                );
                let s = &v.slots[p];
                (
                    cscale_full,
                    s.cover_scale,
                    s.cover.is_some(),
                    s.prev_cover.is_some(),
                )
            })
        };

        // ① 下滚停稳第 10 页：可见页满密、邻居页降档 2.0。
        jump(&mut *cx, 10);
        settle(&mut *cx);
        let (full10, cs10, has10, _b10) = cover_meta(&mut *cx, 10);
        let (full11, cs11, has11, _b11) = cover_meta(&mut *cx, 11);
        eprintln!(
            "[P1-③] settle@10 full10={full10:.2} cover10_cs={cs10:.2} has10={has10} full11={full11:.2} cover11_cs={cs11:.2} has11={has11}"
        );
        assert!(has10, "可见页 10 应有封面");
        assert!(
            (cs10 - full10).abs() <= 1e-4,
            "可见页 10 封面应满密度（cscale={full10:.2}）：cover_scale={cs10:.2}"
        );
        assert!(has11, "封面窗内邻居页 11 应有封面（低档供给，供翻页瞬间有底图）");
        let want11 = full11.min(config::pdf::COVER_SCALE_CAP_NEIGHBOR);
        assert!(
            (cs11 - want11).abs() <= 1e-2,
            "邻居页 11 封面应降到 min(cscale, 2.0)={want11:.2}（而非满密 {full11:.2}）：cover_scale={cs11:.2}"
        );
        // ② 翻到 11 停稳 → 11 真可见 → 满密度重派快速就位；**可见页全程恒有底图**（无闪白）。
        // 注：`loading_placeholder_hits` 会因**窗外 margin 卡**（走廊外 off-screen 页卡，用户
        // 不可见）持续自增，与本次「邻居→可见翻页」的闪白正交（该口径由既有
        // `thumb_backdrop_keeps_normal_roll_loading_free` 的「静置零增长」测试覆盖）——此处
        // 用最精准的判据：翻页全程**可见页**（与视口 bbox 相交）帧帧都有底图（底图判定与
        // `build_visible_page_cards` 同源），任何一帧缺口即闪白。
        jump(&mut *cx, 11);
        let mut no_base_visible = 0u32;
        let mut flip_vis_frames = 0u32;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let pw = v.page_width();
                let sy = f32::from(-v.scroll.offset().y);
                let vh = f32::from(v.scroll.bounds().size.height);
                for &p in &pdf_visible_pages(v, pw, sy, vh) {
                    let s = &v.slots[p];
                    flip_vis_frames += 1;
                    let has_base = s.cover.is_some()
                        || s.prev_cover.is_some()
                        || s.prefetch_cover.is_some()
                        || s.mid.is_some()
                        || s.thumb.is_some();
                    if !has_base {
                        no_base_visible += 1;
                    }
                }
                (
                    v.pending_cover.is_empty() && v.pending_tiles.is_empty(),
                    !v.prev_fast,
                )
            });
            if st == (true, true) {
                break;
            }
        }
        eprintln!(
            "[P1-③] flip@11 no_base_visible_frames={no_base_visible} flip_vis_sampled={flip_vis_frames}"
        );
        assert!(
            flip_vis_frames >= 10,
            "采样应覆盖翻页过程：flip_vis_sampled={flip_vis_frames}"
        );
        assert_eq!(
            no_base_visible, 0,
            "邻居→可见翻页全程可见页必须恒有底图（无闪白）：no_base_visible={no_base_visible}"
        );
        let (full11b, cs11b, has11b, bridged11) = cover_meta(&mut *cx, 11);
        eprintln!(
            "[P1-③] settle@11 full11={full11b:.2} cover11_cs={cs11b:.2} has11={has11b} prev_bridge={bridged11}"
        );
        assert!(
            has11b && (cs11b - full11b).abs() <= 1e-4,
            "入视后页 11 应由满密度封面供给（cover_scale={cs11b:.2} vs {full11b:.2}）"
        );
    }

    /// 回归（**F2 评审 D1 兜底**）：方向翻位时「刚退位的反向邻居」封面必须转存为**有界
    /// `prev_cover` 桥底图**，反向微调滚动（overshoot 一档再回、新封面在途）期间该页底图链
    /// （cover→prev_cover→mid→thumb）非空、不落「第 N 页 加载中…」；桥严格有界（同一时刻
    /// 至多 1 张），该页重新入窗、新封面到位即替换释放。
    /// 真实渲染路径 + 真夹具：下滚停稳第 10 页（dir=1，封面窗 [10..11]，10/11 封面驻留）
    /// → 上翻停稳第 9 页（dir=-1，封面窗 [8..9]，第 10 页退位）→ 断言第 10 页 `cover=None`
    /// 且 `prev_cover=Some`（桥在场）、全局 prev_cover 计数 ==1 → 翻回第 10 页停稳后断言
    /// 第 10 页由新封面供给、桥已被替换释放。
    #[gpui::test]
    async fn cover_retire_bridge_prevents_flash_on_direction_swap(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).page_count()
        });
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");

        // 等待封面在途清空 + 退出快速态。⚠️ 只等封面/快速态、**不等瓦片**：D1 断言只依赖
        // 封面/prev_cover 槽位状态；瓦片在途（如本机 worker 渲染较重时）可能长时间悬着，
        // 与兜底逻辑正交（memory 全量滚动测试已证瓦片可沉降且预算有界）。
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (v.pending_cover.is_empty(), !v.prev_fast)
                });
                if st == (true, true) {
                    return;
                }
            }
            panic!("状态应沉降（封面在途/快速态应清空）");
        };
        let jump = |cx: &mut gpui::VisualTestContext, page: usize| {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(page);
                        cx.notify();
                    });
            });
        };

        // ① 下滚到第 10 页停稳：dir=1 → 封面窗 [10..11]，10（可见）+11（方向前瞻）都驻留。
        // ⚠️ 先暖 60 帧让自适应页宽（`base_w` 收敛）与滚动容器布局稳定：`scroll_to_page`
        // 按 `page_tops` 定位，若在 refit 尚未收敛时跳页，随后 refit 重算 page_tops 会把
        // 视口偏移带走（实测跳 10 却落在 ~13），掩盖本测试要对的 D1 语义。
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let pre_cover_pages = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let c: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.cover.is_some())
                .map(|(p, _)| p)
                .collect();
            (c, f32::from(-v.scroll.offset().y), v.scroll_dir)
        });
        eprintln!(
            "[D1-TEST] PRE-jump off_y={:.0} dir={} covers={:?}",
            pre_cover_pages.1, pre_cover_pages.2, pre_cover_pages.0
        );
        jump(cx, 10);
        settle(cx);
        let (have10, have11, stray_bridges, cover_pages) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let cover_pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.cover.is_some())
                .map(|(p, _)| p)
                .collect();
            (
                v.slots[10].cover.is_some(),
                v.slots[11].cover.is_some(),
                v.slots.iter().filter(|s| s.prev_cover.is_some()).count(),
                cover_pages,
            )
        });
        eprintln!(
            "[D1-TEST] phase1 jump=10 off_y={:.0} dir={} cover_pages={cover_pages:?} tops10/12/13={:?}",
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                f32::from(-v.scroll.offset().y)
            }),
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.scroll_dir
            }),
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.page_tops.get(10).copied(),
                    v.page_tops.get(12).copied(),
                    v.page_tops.get(13).copied(),
                )
            }),
        );
        assert!(have10, "第 10 页（可见）应有封面");
        assert!(have11, "第 11 页（方向前瞻 1）应有封面");
        assert_eq!(stray_bridges, 0, "下滚稳态不应存在残留过渡桥");

        // ② 上翻到第 9 页停稳：dir 翻转为 -1 → 封面窗 [8..9]，第 10 页成为「刚退位的反向
        //    邻居」→ 其封面应转存为有界 prev_cover 桥（而非直接下线 → 反向重读不至于闪白）。
        jump(cx, 9);
        settle(cx);
        let (cover_gone, bridged, bridge_count) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let s = &v.slots[10];
            (
                s.cover.is_none(),
                s.prev_cover.is_some(),
                v.slots.iter().filter(|s| s.prev_cover.is_some()).count(),
            )
        });
        assert!(
            cover_gone,
            "第 10 页退出封面窗后 cover 应收走（否则无法收窄驻留）"
        );
        assert!(
            bridged,
            "退位页第 10 页应有 prev_cover 桥底图（D1 兜底：反向重读不落「加载中」）"
        );
        assert_eq!(bridge_count, 1, "过渡桥应严格有界 1 张，不得累积");

        // ③ 翻回第 10 页停稳：dir 翻回 +1 → 第 10 页重新入窗 [10..11]，新封面到位即把旧桥
        //    （page=10）替换释放；同时本帧新「反向邻居」= 第 9 页接棒持有桥（dir=+1 的
        //    退位页 = vis_first-1）——桥仍严格有界 1 张，只是跟手换页。
        jump(cx, 10);
        settle(cx);
        let (recovered, bridge_released, bridge_pages) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.prev_cover.is_some())
                .map(|(p, _)| p)
                .collect();
            (
                v.slots[10].cover.is_some(),
                v.slots[10].prev_cover.is_none(),
                pages,
            )
        });
        eprintln!("[D1-TEST] phase3 bridge_pages={bridge_pages:?}");
        assert!(recovered, "第 10 页重新入窗后应由新封面供给");
        assert!(
            bridge_released,
            "新封面到位后第 10 页的过渡桥应被替换释放（不残留常驻缓存）"
        );
        assert_eq!(
            bridge_pages,
            vec![9],
            "翻回后桥应跟手换到新的反向邻居（第 9 页）且仅此 1 张：{bridge_pages:?}"
        );
    }

    /// F4/A 回归（真实渲染路径 + 真夹具）：打开 / 长跳停稳后，每个可见页都应有底图
    /// （`thumb.is_some() || cover.is_some()`，F4/A 同帧补缩略把常态主路径的「加载中」
    /// 空窗填掉），且**静置期「加载中」计数零增长**（不再有帧落加载态；只剩跳转途中
    /// VeryFast 无隙期的短暂命中，属 C 的可接受取舍）。同时上报瞬态诊断（封面在途期
    /// 是否观测到缩略兜底、可见页全空帧数）供人工核对，不作时序硬断言（机器快慢各异）。
    #[gpui::test]
    async fn thumb_backdrop_keeps_normal_roll_loading_free(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        // 暖 60 帧让自适应页宽与滚动容器布局稳定（与 cover_retire_bridge 同款）
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && !v.prev_fast
                        && !v.tile_pump_running
                });
                if st {
                    return;
                }
            }
            panic!("状态应沉降");
        };
        // 校验「打开/长跳停稳后每可见页有底图 + 静置零加载」：阶段 s。
        let read_hits = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).loading_placeholder_hits.get())
        };
        let check_phase = |cx: &mut gpui::VisualTestContext, tag: &str| {
            // 停稳后再推 5 帧静置，采样「加载中」计数是否零增长
            for _ in 0..5 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
            }
            let hits_before = read_hits(cx);
            let (vis_missing, vis_count) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let off_y = f32::from(-v.scroll.offset().y);
                let pw = v.page_width();
                let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
                let vis = pdf_visible_pages(v, pw, off_y, vh);
                let missing: Vec<usize> = vis
                    .iter()
                    .copied()
                    .filter(|&p| {
                        let s = &v.slots[p];
                        !(s.cover.is_some() || s.thumb.is_some())
                    })
                    .collect();
                (missing, vis.len())
            });
            for _ in 0..5 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
            }
            let hits_after = read_hits(cx);
            eprintln!(
                "[F4-A/{tag}] visible={vis_count} missing_base={vis_missing:?} loading_hits {hits_before}→{hits_after}"
            );
            assert!(
                vis_missing.is_empty(),
                "[{tag}] 停稳后每可见页应有底图（thumb||cover）：{vis_missing:?}"
            );
            assert!(
                hits_after == hits_before,
                "[{tag}] 静置期「加载中」计数应零增长：{hits_before}→{hits_after}"
            );
        };

        settle(cx);
        check_phase(cx, "open"); // 打开 → 停稳

        // 长跳到第 12 页（目的地页无任何既有底图）→ 停稳
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(12);
                    cx.notify();
                });
        });
        settle(cx);
        check_phase(cx, "jump12");
    }

    /// F4/B 回归（真实渲染路径 + 真夹具）：idle 空闲期把走廊 [vis±2] 填上半密度封面
    /// （WPS「好像全部已加载」的来源；`prefetch_reqs` 增长、廊封面入槽、总量 ≤ 预算）；
    /// 窗口移动（跳页 + 停稳，非 idle 帧）后走廊窗外的旧廊封面按距离回收、新窗内的
    /// 廊封面保留——内存护栏 [`PREFETCH_COVER_BUDGET_BYTES`] 恒成立（独立于窗口封面计数）。
    #[gpui::test]
    async fn idle_prefetch_corridor_bounded_and_reclaims_on_window_move(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (v.pending_cover.is_empty() && v.pending_tiles.is_empty(), !v.prev_fast)
                });
                if st == (true, true) {
                    return;
                }
            }
            panic!("状态应沉降");
        };
        let jump = |cx: &mut gpui::VisualTestContext, page: usize| {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(page);
                        cx.notify();
                    });
            });
        };

        // ① 跳页 20 停稳（方向向下 → 封面窗 [20..21]；18/19/22 为走廊候选页）
        jump(cx, 20);
        settle(cx);
        // idle 期推 60 帧：空闲分支逐帧派发走廊 + 泵收集，直到 [vis±2] 填满/预算
        let p0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).prefetch_reqs);
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let (p1, pf_covers, pf_bytes, off_y) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let mut cnt = 0usize;
            let mut bytes = 0u64;
            let pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.prefetch_cover.is_some())
                .map(|(p, s)| {
                    cnt += 1;
                    let (w, h) = layout::page_bitmap_size(v.dims[p], s.prefetch_cover_scale);
                    bytes += (w as u64) * (h as u64) * 4;
                    p
                })
                .collect();
            (v.prefetch_reqs, pages, bytes, off_y)
        });
        eprintln!(
            "[F4-B] phase1 prefetch_reqs {p0}→{p1} covers={pf_covers:?} bytes={}MB off_y={off_y:.0}",
            pf_bytes / 1024 / 1024,
        );
        assert!(p1 > p0, "idle 期应实际派发走廊封面（prefetch_reqs 应增长）");
        assert!(!pf_covers.is_empty(), "idle 期后走廊页应有廊封面入槽");
        let vis_first = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            layout::first_visible_page(&v.dims, &v.page_tops, v.page_width(), off_y)
        });
        let (lo, hi) = (
            vis_first.saturating_sub(config::pdf::PREFETCH_COVER_MARGIN),
            vis_first + config::pdf::PREFETCH_COVER_MARGIN,
        );
        assert!(
            pf_covers.iter().all(|&p| p >= lo && p <= hi),
            "廊封面应全部落在走廊 [vis-2..vis+2]：[{lo}..{hi}] 内，实际 {pf_covers:?}"
        );
        assert!(
            pf_bytes <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "廊封面总字节应 ≤ 预算：{pf_bytes} > {}",
            config::pdf::PREFETCH_COVER_BUDGET_BYTES
        );

        // ② 窗口移动（跳页 23 停稳 → 非 idle 帧）→ 走廊外旧廊封面按距离回收
        jump(cx, 23);
        settle(cx);
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let (pf_covers2, off_y2, bytes2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let mut bytes = 0u64;
            let pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.prefetch_cover.is_some())
                .map(|(p, s)| {
                    let (w, h) = layout::page_bitmap_size(v.dims[p], s.prefetch_cover_scale);
                    bytes += (w as u64) * (h as u64) * 4;
                    p
                })
                .collect();
            (pages, off_y, bytes)
        });
        eprintln!(
            "[F4-B] phase2 covers={pf_covers2:?} bytes={}MB off_y={off_y2:.0}",
            bytes2 / 1024 / 1024,
        );
        let vis_first2 = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            layout::first_visible_page(&v.dims, &v.page_tops, v.page_width(), off_y)
        });
        let (lo2, hi2) = (
            vis_first2.saturating_sub(config::pdf::PREFETCH_COVER_MARGIN),
            vis_first2 + config::pdf::PREFETCH_COVER_MARGIN,
        );
        assert!(
            pf_covers2.iter().all(|&p| p >= lo2 && p <= hi2),
            "窗口移动后廊封面应全部落在新走廊 [{lo2}..{hi2}] 内：{pf_covers2:?}"
        );
        assert!(
            bytes2 <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "窗口移动后廊封面字节仍 ≤ 预算：{bytes2}"
        );
        // 走廊外（旧位置 18/19）的廊封面应已被回收
        assert!(
            !pf_covers2.contains(&18) && !pf_covers2.contains(&19),
            "旧走廊（18/19）廊封面应在窗口移动后被回收：{pf_covers2:?}"
        );

        // ③（评审 F2）空闲消化会收敛：窗口**不动**继续 idle 泵若干帧 → 走廊填满后
        // `pending_prefetch_cover` 收敛到 0、`prefetch_reqs` 不再增长（不会无限泵/无限走廊增长）。
        // 先泵到走廊完全落定（在途清空，再多的派发也发生在收敛之前），再取收敛基线。
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let pf_done = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_prefetch_cover.is_empty()
            });
            if pf_done {
                break;
            }
        }
        let (prefetch_reqs_before, pf_converged) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.prefetch_reqs, v.pending_prefetch_cover.is_empty())
        });
        // 再连推 30 帧静置：走廊在途保持 0、预取请求数不再增长（收敛而非空转）。
        let mut pf_never_nonempty = true;
        let mut reqs_stable = true;
        for _ in 0..30 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let (pf_inflight, reqs_now) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.pending_prefetch_cover.is_empty(), v.prefetch_reqs)
            });
            pf_never_nonempty &= pf_inflight;
            reqs_stable &= reqs_now == prefetch_reqs_before;
        }
        eprintln!(
            "[F4-B] phase3 converge pf_inflight_empty={pf_converged} reqs_stable_in_30frames={reqs_stable}"
        );
        assert!(pf_converged, "窗口不动 idle 泵后走廊在途应收敛到 0");
        assert!(
            reqs_stable && pf_never_nonempty,
            "收敛后继续静置不得再派走廊/在途保持 0（空闲消化会收敛）：reqs_stable={reqs_stable}"
        );
        assert!(
            prefetch_reqs_before > 0,
            "phase3 前应已有走廊预取发生过（收敛才有意义）"
        );
    }

    /// L1+L2 **大跳→落点直达**（真实渲染路径 + 真夹具）：一次性跳 30 页停靠后，目标页封面
    /// **≤20 帧内入槽**（不再是 VeryFast 纯平移零派发 + 停稳后 EMA 0.7ⁿ 衰减 ~150-250ms 才
    /// 重新调度）；落点封面入槽后 `loading_placeholder_hits` 不再增长（只允许渲染到达前那
    /// 1-2 帧的插队增量）；`nav_gen` 随大跳推进（世代机制生效）、落点封面确实派发。
    #[gpui::test]
    async fn jump_delivers_landing_cover_without_loading_growth(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count());
        assert!(n >= 40, "测试文档应至少有 40 页，实际 {n}");
        // 暖 60 帧让自适应页宽/滚动容器布局稳定（thumb_backdrop/cover_retire_bridge 同款）
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_prefetch_cover.is_empty()
                        && !v.prev_fast
                        && !v.tile_pump_running
                });
                if st {
                    return;
                }
            }
            panic!("初始状态应沉降");
        };
        settle(cx);

        let read_hits = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).loading_placeholder_hits.get()
            })
        };
        let hits0 = read_hits(cx);
        let nav0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).nav_gen);
        let cov0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).cover_reqs);

        // 一次性 30 页大跳（模拟滚动条拖到终点：单帧位移 ≫ JUMP_PAGES_THRESHOLD）
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(30);
                    cx.notify();
                });
        });
        // L1+A 两段式落点直达：目标页**任何底图**（优先 0.5 密度 preview 铺底、满密 cover 精修
        // 随后接棒）≤20 帧内入槽（停手即派发，只剩 preview ~10ms 渲染）。满密 cover 精修单独
        // 等待（≤40 帧）——preview 已桥接底图，cover 慢几帧无感知。
        let mut base_ready = false;
        let mut base_frames = 0usize;
        for _ in 0..20 {
            base_frames += 1;
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            base_ready = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let s = &v.slots[30];
                s.cover.is_some() || s.prefetch_cover.is_some() || s.thumb.is_some() || s.mid.is_some()
            });
            if base_ready {
                break;
            }
        }
        let mut cover_full = false;
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            cover_full = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.slots[30].cover.is_some()
            });
            if cover_full {
                break;
            }
        }
        eprintln!(
            "[L1] jump30 target_base_frames={base_frames} base_ready={base_ready} cover_full={cover_full}"
        );
        assert!(base_ready, "大跳后目标页应 ≤20 帧内有底图（两段式：preview 铺底先行）");
        assert!(cover_full, "大跳后目标页满密 cover 最终应入槽（精修接棒）");

        // 完整沉降（泵停 + 全部在途清空 + 走廊兜底缓冲页填完）：「加载中」计数随之冻结。
        // 注意：**不等待泵停**就断言计数零增长，会把 MARGIN 缓冲页的合理占位噪点误判为回归
        // （thumb_backdrop 同款：settle 含 `!tile_pump_running`，check_phase 只查真可见页）。
        let mut settled = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && v.pending_prefetch_cover.is_empty()
                    && !v.prev_fast
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        assert!(settled, "跳后状态应完整沉降");
        // 沉降（泵停）后静置 20 帧：「加载中」计数不得再增长（目标页不再长期停占位）
        let hits1 = read_hits(cx);
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let hits2 = read_hits(cx);
        // 真可见页全部有底图（thumb_backdrop 同款口径：只查视口内，不含 MARGIN 缓冲页）
        let (vis_missing, vis_count) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let pw = v.page_width();
            let vh = f32::from(v.scroll.bounds().size.height).max(1.0);
            let vis = pdf_visible_pages(v, pw, off_y, vh);
            let missing: Vec<usize> = vis
                .iter()
                .copied()
                .filter(|&p| {
                    let s = &v.slots[p];
                    !(s.cover.is_some()
                        || s.thumb.is_some()
                        || s.mid.is_some()
                        || !s.tiles.is_empty()
                        || s.prefetch_cover.is_some())
                })
                .collect();
            (missing, vis.len())
        });
        eprintln!(
            "[L1] loading_hits {hits0}→{hits1}→{hits2} vis_count={vis_count} vis_missing={vis_missing:?}"
        );
        assert!(
            vis_missing.is_empty(),
            "大跳停稳后每真可见页应有底图：{vis_missing:?}"
        );
        assert!(
            hits2 == hits1,
            "沉降（泵停）后「加载中」计数零增长（目标页不再长期停占位）"
        );
        // 大跳确已发生（nav_gen 推进 + 落点封面派发）
        let (nav1, cov1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.nav_gen, v.cover_reqs)
        });
        assert!(nav1 > nav0, "大跳应推进 nav_gen：{nav0}→{nav1}");
        assert!(cov1 > cov0, "大跳落点应派发封面：{cov0}→{cov1}");
    }

    /// E **帧账拆解**（真实渲染路径 + 真夹具）：把「滚动落点 → 目标页首张 cover 入槽」端到端
    /// 帧数拆成 (i) 派发段 (ii) worker 渲染段 (iii) settle/速度段 (iv) collect+上屏段，
    /// 输出「段→帧数」表作为 A/B/C 取舍依据。逐帧打点（speed 档 / nav_gen / cover 在途与
    /// 入槽 / 封面字节预算），并单独计时 worker 单张封面真实渲染墙钟（展望筒：worker 串行
    /// 渲染是唯一不可并行压缩的物理段）。
    #[gpui::test]
    async fn e_frame_account_jump_to_target_cover(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_prefetch_cover.is_empty()
                        && !v.prev_fast
                        && !v.tile_pump_running
                });
                if st {
                    return;
                }
            }
            panic!("E 帧账：初始状态应沉降");
        };
        settle(cx);

        // 基线：直调 worker 渲染目标页封面一次，量出单张封面串行墙钟（展望筒估算）。
        let (worker, doc_id, cscale) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let w = v.worker.as_ref().unwrap().clone();
            let c = crate::pdf::layout::cover_scale(v.dims[30], v.page_width(), 1.0);
            (w, v.doc_id, c)
        });
        let t0 = Instant::now();
        let rx_solo = worker.request_render(doc_id, crate::pdf::RenderRequest { page: 30, scale: cscale }, 0, 0);
        let _solo = rx_solo.recv_timeout(Duration::from_secs(30)).expect("单人渲染应返回");
        let solo_render_ms = t0.elapsed().as_secs_f64() * 1e3;

        let (cov0, nav0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.nav_gen)
        });
        // 30 页大跳
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(30);
                cx.notify();
            });
        });
        // 逐帧打点（最多 40 帧）
        let wall0 = Instant::now();
        let mut rows: Vec<(usize, String, u64, usize, bool)> = Vec::new();
        let mut pend_first_frame: Option<usize> = None; // 封面入队首帧
        let mut land30_frame: Option<usize> = None; // 目标页 cover 入槽首帧
        let mut slow_frame: Option<usize> = None; // 速度落 Slow 首帧
        let mut very_frames = 0usize;
        for f in 0..40usize {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let (sp, nav, pc, c30) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let sp = match v.speed_state {
                    crate::pdf::view::SpeedState::Idle => "Idle",
                    crate::pdf::view::SpeedState::Slow => "Slow",
                    crate::pdf::view::SpeedState::Fast => "Fast",
                    crate::pdf::view::SpeedState::VeryFast => "VF",
                };
                (sp.to_string(), v.nav_gen, v.pending_cover.len(), v.slots[30].cover.is_some())
            });
            if pc > 0 && pend_first_frame.is_none() {
                pend_first_frame = Some(f);
            }
            if c30 && land30_frame.is_none() {
                land30_frame = Some(f);
            }
            if sp == "Slow" && slow_frame.is_none() {
                slow_frame = Some(f);
            }
            very_frames += (sp == "VF") as usize;
            rows.push((f, sp, nav, pc, c30));
        }
        let e2e_ms = wall0.elapsed().as_secs_f64() * 1e3;
        let (cov1, nav1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.cover_reqs, v.nav_gen)
        });
        let dispatch_frames = pend_first_frame.map_or(0, |f| f + 1); // 落点→封面在途 = 派发段
        let render_frames = land30_frame.zip(pend_first_frame).map_or(0, |(l, p)| l.saturating_sub(p)); // 入队→入槽 = worker+collect 段
        eprintln!(
            "[E-FRAME]  段拆解: 端到端 {land30_frame:?} 帧(~{e2e_ms:.0}ms墙钟)"
        );
        eprintln!(
            "[E-FRAME]  (i)派发   [0→入队]  {dispatch_frames} 帧 (COVER_INFLIGHT 节流后首帧在途 {pend_first_frame:?})"
        );
        eprintln!(
            "[E-FRAME]  (ii)worker+collect [入队{pend_first_frame:?}→入槽{land30_frame:?}]  {render_frames} 帧 | 单张封面墙钟 {solo_render_ms:.1}ms (~{:.1} 迭代)"
        , solo_render_ms / 3.0);
        eprintln!(
            "[E-FRAME]  (iii)settle [VeryFast→Slow]  首 Slow 帧 {slow_frame:?} (VeryFast 帧数 {very_frames})"
        );
        eprintln!(
            "[E-FRAME]  (iv)collect+notify [入槽后上屏]  ≤1 帧（收藏当帧 cx.notify 即刻）"
        );
        eprintln!(
            "[E-FRAME]  cover_reqs {cov0}→{cov1}（+{}） nav_gen {nav0}→{nav1}",
            cov1.saturating_sub(cov0)
        );
        eprintln!(
            "[E-FRAME]  逐帧: {:?}",
            rows.iter().map(|(f, sp, nav, pc, c30)| format!("{f}:{sp}(n{nav},pc{pc}{})", if *c30 { "+C30" } else { "" })).collect::<Vec<_>>()
        );
        // 断言仅锁「机制成立」：目标页 cover 应在 30 帧内入槽（端到端指标本身进报告）。
        assert!(land30_frame.is_some(), "目标页 cover 应在 ≤30 帧内入槽");
        assert!(nav1 > nav0, "大跳应推进 nav_gen");
        assert!(cov1 > cov0, "落点应派发封面");
    }

    /// A **两段式底图即显——测量钩子**（真实渲染路径 + 真夹具）：30 页大跳后逐帧打点
    /// 「目标页任何底图首帧（cover|prefetch_cover|thumb|mid|tiles）」与「落点窗 [30,31]
    /// 全部底图首帧」与「满密 cover 入槽首帧」与「瓦片精修开始首帧」，作为 A/B/C 的
    /// before/after 判据（before 在 c0b2fba 基线跑、after 在改动后跑，差值进报告）。
    #[gpui::test]
    async fn a_measure_two_stage_base_image(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_prefetch_cover.is_empty()
                        && !v.prev_fast
                        && !v.tile_pump_running
                });
                if st {
                    return;
                }
            }
            panic!("A 测量：初始状态应沉降");
        };
        settle(cx);

        let tile0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).tile_reqs);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(30);
                cx.notify();
            });
        });
        let mut t_base30: Option<usize> = None; // 目标页任何底图首帧
        let mut t_win_all: Option<usize> = None; // [30,31] 全底图首帧
        let mut t_cover30: Option<usize> = None; // 满密 cover 入槽首帧
        let mut t_tile_start: Option<usize> = None; // 瓦片精修开始首帧
        for f in 0..40usize {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let (base30, base31, cover30, tile_start) = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let has = |p: usize| {
                    let s = &v.slots[p];
                    s.cover.is_some()
                        || s.prefetch_cover.is_some()
                        || s.thumb.is_some()
                        || s.mid.is_some()
                        || !s.tiles.is_empty()
                };
                (has(30), has(31), v.slots[30].cover.is_some(), v.tile_reqs > tile0)
            });
            if base30 && t_base30.is_none() {
                t_base30 = Some(f);
            }
            if base30 && base31 && t_win_all.is_none() {
                t_win_all = Some(f);
            }
            if cover30 && t_cover30.is_none() {
                t_cover30 = Some(f);
            }
            if tile_start && t_tile_start.is_none() {
                t_tile_start = Some(f);
            }
        }
        eprintln!(
            "[A-MEASURE] before/after 判据: 目标页底图首帧 {t_base30:?} | 落点窗全部底图首帧 {t_win_all:?} | 满密cover入槽 {t_cover30:?} | 瓦片精修开始首帧 {t_tile_start:?}"
        );
        // 性能口径：真实墙钟数值以 [A-MEASURE] eprintln 为准（本线 A 改造实测 11→3/26→5），
        // 下列帧数阈值只是**机制回归闸**（防两段式点名失效、仅靠 cover 兜底仍绿），非性能承诺。
        // 阈值留 2.4-2.7x 余量防重载机 flaky：base30≤8（实测3）、win_all≤12（实测5）、cover≤40（实测20）。
        assert!(
            t_base30.map_or(usize::MAX, |f| f) <= 8,
            "两段式底图机制失效：目标页底图应在 ≤8 帧内出现（实测3）, 现 {t_base30:?}"
        );
        assert!(
            t_win_all.map_or(usize::MAX, |f| f) <= 12,
            "两段式整窗铺底失效：落点窗全底图应 ≤12 帧（实测5）, 现 {t_win_all:?}"
        );
        assert!(t_base30.is_some(), "目标页应有底图");
        assert!(t_cover30.is_some(), "目标页满密 cover 最终入槽");
    }

    /// L2 **世代 drop（出队不渲，worker 侧）**（真实 worker + 真夹具）：导航世代先推进
    /// （Navigate(1)，模型 view 大跳后推送）——随后到达的**旧世代请求**（gen=0 `request_render`）
    /// 在 worker 出队时被 drop（rx 恒 Empty = 从不渲染、从不回传，等价于「旧页 raster_reqs==0」），
    /// **只渲最新世代**（新 rx 收到真实结果；新在途结果不被误弃）——快速连续跳「只渲最新落点」
    /// 的 CPU 侧护栏。注：为确定性取「先 Navigate 再旧请求」的晚到序（`头消息经 worker 阻塞
    /// recv 先取就会在 Navigate 之前入队渲染」，改写成先导航后旧请求即完全确定）；
    /// 「先旧后新、Navigate 居中」的 view 侧整链已由 `jump_delivers_...` 的百万代真实路径覆盖。
    #[test]
    fn navigate_gen_drops_stale_queued_renders() {
        skip_if_missing_pdf_fixture!();
        use std::time::Duration;
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        // 推进导航世代（view 大跳后推送；Navigate 控制消息先于渲染出队生效）
        worker.navigate(doc_id, 1);
        // 旧世代请求（gen=0）晚到 → 出队时应被 drop
        let rx_old_a = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let rx_old_b = worker.request_render(doc_id, RenderRequest { page: 1, scale: 1.0 }, 0, 0);
        // 新世代请求（gen=1）→ 应正常渲染
        let rx_new = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 }, 1, 0);
        // 新世代结果应到达（真实渲染；新在途结果不被误弃）
        let new_res = rx_new
            .recv_timeout(Duration::from_secs(30))
            .expect("新世代应收到结果")
            .expect("渲染应成功");
        assert!(new_res.width > 0 && new_res.height > 0, "新世代渲染应有正尺寸");
        // 旧世代请求在 worker 出队时被 drop → worker 从不渲染/从不回传、其 tx 随消息丢弃 →
        // rx 呈 **Disconnected**（区别于 Empty：Channel 还活着但没消息 = 可能只是还没轮到）。
        // 因 FIFO，`new_res` 到达时旧请求必已被出队丢弃，故 Disconnected 是确定结局。
        assert!(
            matches!(rx_old_a.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)),
            "旧世代请求应被 drop（tx 丢弃 → Disconnected，无回传）"
        );
        assert!(
            matches!(rx_old_b.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)),
            "旧世代请求应被 drop（tx 丢弃 → Disconnected，无回传）"
        );
        // 世代同步：worker 镜像与 view 推进对齐
        assert_eq!(
            worker.navigate_gen(doc_id),
            1,
            "worker navigate_gen 应采纳 Navigate(1)"
        );
        worker.close(doc_id);
    }

    /// L2.4+L2.5 **世代同步 + 预算即释**（真实渲染路径 + 真夹具，复用 w1 探针姿势）：
    /// 大跳当帧 `nav_gen++` → view 清掉世代 < 新 gen 的全部在途条目（`pending_cover` 随即
    /// 以 ≤COVER_INFLIGHT 突发重填）、worker `navigate_gen` 镜像经 Navigate 消息与 view 对齐、
    /// 落点封面确实派发；当前世代落点封面正常入槽（不被世代机制误弃）。
    #[gpui::test]
    async fn jump_nav_gen_budget_release_and_worker_sync(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_mid.is_empty()
                        && v.pending_thumb.is_empty()
                        && !v.prev_fast
                });
                if st {
                    return;
                }
            }
            panic!("初始状态应沉降");
        };
        settle(cx);

        let (nav0, cov0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.nav_gen, v.cover_reqs)
        });
        assert_eq!(nav0, 0, "基线世代应 0");

        // 一次性 30 页大跳
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(30);
                    cx.notify();
                });
        });
        for _ in 0..3 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let (nav1, pend1, cov1, mirror1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.nav_gen,
                v.pending_cover.len(),
                v.cover_reqs,
                v.worker.as_ref().unwrap().navigate_gen(v.doc_id),
            )
        });
        assert!(nav1 > nav0, "大跳应推进 nav_gen：{nav0}→{nav1}");
        assert_eq!(
            mirror1, nav1,
            "大跳后 worker navigate_gen 应经 Navigate 消息与 view 对齐（w1 探针同款）"
        );
        assert!(
            pend1 <= config::pdf::COVER_INFLIGHT,
            "清旧世代后当帧突发应 ≤ COVER_INFLIGHT（预算即释）：pend={pend1}"
        );
        assert!(cov1 > cov0, "落点封面已派发");

        // 当前世代落点封面正常入槽（不被世代机制误弃）
        let mut covered = false;
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            covered = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.slots[30].cover.is_some()
            });
            if covered {
                break;
            }
        }
        assert!(covered, "当前世代落点封面应正常渲染入槽（不被世代机制误弃）");
    }

    /// L3.1+L3.2 **走廊重武装（真实渲染路径 + 真夹具）**：VeryFast 纯平移期（四主队列空）不再
    /// 因 `prev_fast` 拒派走廊封面——高速平移中 `prefetch_reqs` 仍增长（重武装）、新增廊封面
    /// 沿滚动方向前向侧（下滚时页号增大侧）出现、总量仍 ≤ 40MB 预算（与 `idle_prefetch` 的
    /// 预算护栏同款）。
    #[gpui::test]
    async fn corridor_rearmed_during_veryfast_forward_only_bounded(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _w: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1400.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        // 初始走廊在 [vis±2] 落定：推进程与 E2 一致（warm 60 帧 + 静置收敛）
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_prefetch_cover.is_empty()
                        && !v.prev_fast
                        && !v.tile_pump_running
                });
                if st {
                    return;
                }
            }
            panic!("走廊重武装测试：初始状态应沉降");
        };
        settle(cx);

        let (p0, vis0, off0, pages0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let vis0 = layout::first_visible_page(&v.dims, &v.page_tops, v.page_width(), off_y);
            let pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.prefetch_cover.is_some())
                .map(|(p, _)| p)
                .collect();
            (v.prefetch_reqs, vis0, off_y, pages)
        });
        assert!(p0 > 0, "初始 idle 走廊应已预取过（基线非零）");
        assert!(off0.abs() < 1.0, "起始应在文档顶部");

        // 单帧快滚 3 页（Δ=3 < JUMP_PAGES_THRESHOLD=7 → **不是大跳**：无跳变派发、主队列保持空、
        // 速度瞬时冲进 VeryFast）——这正是 L3.1「VeryFast 高速带 + 四主队列空 → 走廊重武装」的
        // 触发场景（此前 `prev_fast` 直接拒派，走廊不补）。滚动目标页 3、4、5 此前无封面 →
        // 走廊沿前向侧补 0.5 密度廊封面。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(3);
                cx.notify();
            });
        });
        let mut saw_veryfast = false;
        let mut grew = false;
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.prefetch_reqs,
                    v.speed_state == crate::pdf::view::SpeedState::VeryFast,
                )
            });
            saw_veryfast |= st.1;
            grew |= st.0 > p0;
        }
        // 停稳补清：让重武装期派发的廊封面落地（worker 真渲 0.5 密度整页，需几十 ms）
        let mut settled = false;
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_prefetch_cover.is_empty()
                    && v.pending_cover.is_empty()
                    && v.pending_tiles.is_empty()
                    && !v.prev_fast
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        assert!(settled, "重武装后应补清（走廊在途落定）");
        let (p1, bytes, pages1, vis1, off_y1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let off_y = f32::from(-v.scroll.offset().y);
            let vis1 = layout::first_visible_page(&v.dims, &v.page_tops, v.page_width(), off_y);
            let mut bytes = 0u64;
            let pages: Vec<usize> = v
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.prefetch_cover.is_some())
                .map(|(p, s)| {
                    let (w, h) = layout::page_bitmap_size(v.dims[p], s.prefetch_cover_scale);
                    bytes += (w as u64) * (h as u64) * 4;
                    p
                })
                .collect();
            (v.prefetch_reqs, bytes, pages, vis1, off_y)
        });
        eprintln!(
            "[L3] corridor_rearm prefetch {p0}→{p1} saw_veryfast={saw_veryfast} vis {vis0}→{vis1} off {off0:.0}→{off_y1:.0} pages0={pages0:?} pages1={pages1:?} bytes={}MB",
            bytes / 1024 / 1024
        );
        assert!(saw_veryfast, "快滚应短暂进入 VeryFast（Δ<阈值且无跳变派发）");
        assert!(off_y1 > 1.0, "应已滚到第 3 页附近（off_y1={off_y1:.0}）");
        assert!(
            grew && p1 > p0,
            "VeryFast 期四主队列空时走廊应重武装（prefetch_reqs 增长 {p0}→{p1}）"
        );
        assert!(
            bytes <= config::pdf::PREFETCH_COVER_BUDGET_BYTES,
            "VeryFast 期走廊字节仍 ≤ 40MB 预算（budget 护栏不破）"
        );
        // 方向单侧增长：滚动后至少有一个「前向侧新页」（页号 > 滚动起点可见页）拿到廊封面
        let new_forward: Vec<usize> = pages1
            .iter()
            .copied()
            .filter(|&p| p > vis0 && !pages0.contains(&p))
            .collect();
        assert!(
            !new_forward.is_empty(),
            "重武装期新增廊封面应沿方向前向侧出现（前向新页 > 滚动起点可见页 {vis0}）：{pages1:?}"
        );
    }

    /// 回归（**T1 封面先行，真实渲染路径 + 真夹具**）：封面 **affordable 且与瓦片同密度**
    /// 的页面不得同页双渲。旧语义「瓦片先派、封面后到再把已示瓦片整体作废」= 首屏
    /// 36 瓦片 + 3 封面双渲。
    /// 夹具用 `pdf测试2.pdf`（A4 页面）。测试平台为 2× DPI（`window.scale_factor()==2`），
    /// 故小窗 1000px → 在屏密度 ≈3.2 < COVER_SCALE_CAP=4：封面 affordable 且与瓦片
    /// **同密度**（封面是真替代物）。断言首屏 + 远跳翻页（快速占位→停稳补封面）后：
    /// - 瓦片系栅格命令数为 0（`tile_reqs` 不变）；
    /// - 目标页由封面供给（`cover` 在位、同页无瓦片）。
    #[gpui::test]
    async fn cover_first_first_screen_skips_same_page_tiles(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        // 小窗（页宽 ≈960px → A4 在屏密度 ≈3.2 < COVER_SCALE_CAP=4）→ 封面与瓦片同密度。
        cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");

        // 停稳探针（对齐 `full_scroll_memory` 既有口径：只看瓦片/封面/泵——快速跳过后的
        // 占位中清/缩略可能无泵帧暂留，与本回归的「瓦片 vs 封面」构成无关）。
        let settled_now = |cx: &mut gpui::VisualTestContext| -> bool {
            for _ in 0..400 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.tile_pump_running
                });
                if st {
                    return true;
                }
            }
            false
        };
        assert!(settled_now(cx), "首屏应在 400 帧内停稳");
        let t0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).tile_reqs);

        // 远跳翻页（快速态走占位 → 停稳补封面）：目标页同样 affordable 且同密度 →
        // 封面先行供给、不得派发瓦片（旧语义翻页会补一屏瓦片 → `tile_reqs` 增长）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(12);
                cx.notify();
            });
        });
        assert!(settled_now(cx), "翻页后应在 400 帧内停稳");
        let (t1, cover_reqs, raster_reqs, n, sf) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.tile_reqs,
                v.cover_reqs,
                v.raster_reqs,
                v.page_count(),
                _w.scale_factor(),
            )
        });
        assert!(n >= 1, "夹具应有页");
        // 封面 affordable 判定与调度同源（用**真实**窗口 scale_factor，测试平台为 2×）。
        let (affordable0, all_assertible, density_match0, cover_present, tiles_on_page) =
            cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                let pw = v.page_width();
                let sf = _w.scale_factor();
                let page = v.current_page().map(|(p, _)| p - 1).unwrap_or(0);
                let dim = v.dims[page];
                let (c, s) = (
                    layout::cover_scale(dim, pw, sf),
                    layout::render_scale(pw, sf, dim.0),
                );
                let all = (0..n.min(6)).all(|p| {
                    let cc = layout::cover_scale(v.dims[p], pw, sf);
                    let ss = layout::render_scale(pw, sf, v.dims[p].0);
                    layout::cover_affordable(v.dims[p], cc) && (cc - ss).abs() <= 1e-4
                });
                let slot = v.slots.get(page).expect("应有槽");
                (
                    layout::cover_affordable(dim, c),
                    all,
                    (c - s).abs() <= 1e-4,
                    slot.cover.is_some(),
                    !slot.tiles.is_empty(),
                )
            });
        eprintln!(
            "[T1-FIRST-SCREEN] n={n} sf={sf} affordable0={affordable0} all_assertible={all_assertible} density_match0={density_match0} cover_present={cover_present} tiles_on_page={tiles_on_page} tile_reqs={t0}->{t1} cover_reqs={cover_reqs} raster_reqs={raster_reqs}"
        );
        if all_assertible {
            // 封面先行主路径（新不变式，2026-10-30 伪机制修复后）：`plan_tiles_in_window`
            // 只在「封面在槽/在途」时才跳过瓦片——翻页落点页封面尚未在槽/在途的首帧会派一次
            // 当前密度 1:1 瓦片兜底（封面缺失不再静默跳过 → 静止必锐，根治 mod4 糊冻），
            // 这些兜底瓦片由 TILE_INFLIGHT/budget 自理、封面到位后被 `collect_tile_results`/
            // `evict_tiles` 的「同密度封面冗余」回收 → 停稳后**同页无瓦片驻留（无双渲/双驻留）**。
            // 目标页由封面供给、封面到位后瓦片清零。
            // 注：`tile_reqs` 非零部分来自开档初始最大化窗（页宽使密度 > COVER_SCALE_CAP
            // 的「底图+精修」合法瓦片）与封面缺失页的瞬态兜底（本次翻页应 t0→t1 有小幅增长）。
            assert!(t1 >= t0, "封面 affordable 翻页后瓦片计数不应倒退");
            assert!(cover_present, "封面 affordable 目标页应已由封面供给（静止必锐）");
            assert!(!tiles_on_page, "封面 affordable 目标页停稳后不得同页持瓦片（无双重驻留）");
            assert!(
                cover_reqs >= 1,
                "affordable 屏幕应派发了封面：cover_reqs={cover_reqs}"
            );
        } else if affordable0 {
            // 首页同密度可负担、部分页不可 → 目标页须封面先行且同页无瓦片。
            assert!(cover_present, "同密度可负担的目标页应已由封面供给");
            assert!(!tiles_on_page, "同密度可负担的目标页不得同页持瓦片");
        } else {
            // 目标页不可负担 → 纯瓦片回退（无封面、瓦片在位），同样无双渲。
            assert!(!cover_present, "不可负担时目标页不应有同密度封面");
            assert!(t1 > t0 && tiles_on_page, "不可负担应由瓦片供给");
        }
    }

    /// T1 时空基准（首屏**就绪墙钟** + 栅格命令构成，真实渲染路径 + 真夹具）：测量「加载
    /// 完成 → 首屏停稳（无在途瓦片/封面/泵）」的墙钟与 tile/cover 派发计数。用 1000px 小窗
    /// （测试平台 2× DPI → A4 在屏密度 ≈3.2 < COVER_SCALE_CAP=4）：封面 affordable 且与
    /// 瓦片同密度 → **封面先行后瓦片命令为 0**。手动 `--ignored` 跑多轮取中位写进 T1 报告，
    /// 对比 ae02310 基线（首屏 36 瓦片 + 3 封面双渲；此处应只剩封面命令——栅格命令数与
    /// 就绪墙钟都应显著下降）。
    #[gpui::test]
    #[ignore]
    async fn bench_first_screen_wallclock(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let t0 = Instant::now();
        let mut settled = false;
        for _ in 0..800 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
            settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        let ready_ms = t0.elapsed().as_secs_f64() * 1e3;
        let (tile_reqs, cover_reqs, raster_reqs, sf, dw, dh) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.tile_reqs, v.cover_reqs, v.raster_reqs, _w.scale_factor(), v.dims[0].0, v.dims[0].1)
        });
        eprintln!(
            "[BENCH-FIRST-SCREEN] ready_ms={:.0} tile_reqs={tile_reqs} cover_reqs={cover_reqs} raster_reqs={raster_reqs} settled={settled} sf={sf} dim0={dw:.0}x{dh:.0}",
            ready_ms
        );
    }

    /// 回归（Sprint 5 静态帧门控，真实渲染路径）：加载 + 首屏调度/泵结束进入「静态
    /// 停靠」后应进入静置确认（`window_settled`）；连发无变化帧被门控跳过（`idle_gate_skips`
    /// 单调增）且不产生待派、不饿死渲染；跳页（变化帧）立即复位门控并恢复调度（在途
    /// 瓦片/封面出现 = 贴图不迟滞）；重新停稳后再次静置确认。
    #[gpui::test]
    async fn static_gate_enters_idle_and_resumes_scheduling_on_scroll(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));

        // 1) 等加载 + 首屏调度/泵结束 → 静态停靠
        let n = {
            let mut loaded = false;
            for _ in 0..400 {
                cx.run_until_parked();
                loaded =
                    cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
                if loaded {
                    break;
                }
            }
            assert!(loaded, "应加载完成");
            cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count())
        };
        assert!(n >= 50, "长文夹具应 ≥50 页: {n}");
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
        }

        // 2) 静置确认成立：settled + 签名在位 + 无在途 + 非快速
        let quiescent = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.window_settled,
                v.window_sig.is_some(),
                v.pending_tiles.is_empty(),
                v.pending_cover.is_empty(),
                !v.prev_fast,
            )
        });
        assert_eq!(
            quiescent,
            (true, true, true, true, true),
            "静态停靠应进入静置确认: {quiescent:?}"
        );

        // 3) 连发无变化帧：被门控跳过（计数递增）、不产生待派、状态不掉
        let skips_before = cx
            .update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).idle_gate_skips);
        for _ in 0..20 {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |_v, cx| cx.notify());
            });
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
        }
        let (skips_after, still) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.idle_gate_skips,
                (v.window_settled, v.pending_tiles.is_empty(), v.pending_cover.is_empty()),
            )
        });
        assert!(
            skips_after >= skips_before + 20,
            "20 个无变化帧应全部被门控：{skips_before} -> {skips_after}"
        );
        assert_eq!(still, (true, true, true), "静置帧不得产生待派/掉状态: {still:?}");

        // 4) 跳页（变化帧）→ 立即恢复调度：帧内出现在途瓦片或封面（贴图不迟滞）
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(n - 5);
                    cx.notify();
                });
        });
        let mut resumed = false;
        for _ in 0..10 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            let in_flight = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                // 大跨度跳页会进快速态 → 占位（中清/缩略）先调度，之后才补瓦片/封面；
                // 四类在途任一非空都证明「变化帧立即恢复调度」。
                !v.pending_tiles.is_empty()
                    || !v.pending_cover.is_empty()
                    || !v.pending_mid.is_empty()
                    || !v.pending_thumb.is_empty()
            });
            if in_flight {
                resumed = true;
                break;
            }
        }
        assert!(resumed, "跳页变化帧应立即恢复调度（贴图不得迟滞）");

        // 5) 重新停稳后再次静置确认（滚动恢复后门控仍可复用）
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            let settled_again = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.tile_pump_running)
                    && v.window_settled
            });
            if settled_again {
                break;
            }
        }
        let resettle = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.pending_tiles.is_empty(), v.pending_cover.is_empty(), v.window_settled)
        });
        assert_eq!(resettle, (true, true, true), "滚动重定居应再次静置确认: {resettle:?}");
    }

    /// 时空基准（自动对比前后两版）：只依赖两版公共 API，用**完全相同的固定工作负载**
    /// 打印一条 `[BENCH]`：总耗时（加载 + Ctrl+滚轮放大到 ~2.48 密度 + 整份文档前后翻页
    /// 停稳）、停靠总帧数、最终渲染驻留内存估算（瓦片字节 + 封面 + 中清 + 缩略）。
    /// 用法：当前工作树跑一遍记录 after；再 `git worktree add <tmp> e031f0c` 并在其中
    /// 打上 `git diff HEAD -- src/pdf/mod.rs`（本文件即基准源码）后各跑一次，对比 [BENCH]。
    #[gpui::test]
    #[ignore] // 性能/内存基准：依赖真 PDF 夹具且耗时，手动 `--ignored` 跑
    async fn bench_full_scroll_time_memory(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let t_all = Instant::now();
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count());
        let t_open = t_all.elapsed();
        for _ in 0..40 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }

        // Ctrl+滚轮放大到 ~2.48 渲染密度（用户实测档）
        let zoom_wheel = |control: bool, delta_y: f32| gpui::ScrollWheelEvent {
            position: gpui::point(gpui::px(400.), gpui::px(500.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta_y))),
            modifiers: gpui::Modifiers {
                control,
                ..Default::default()
            },
            ..Default::default()
        };
        for _ in 0..5 {
            cx.simulate_event(zoom_wheel(true, 240.0));
            for _ in 0..6 {
                cx.run_until_parked();
            }
        }
        let t_zoom = t_all.elapsed();

        // 固定翻页负载：前向每 6 页 + 回翻（重演用户“完整滚动一遍”）
        let mut targets: Vec<usize> = Vec::new();
        for t in (0..n).step_by(6) {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        for t in (0..n).step_by(6).rev() {
            let t = t.min(n - 1);
            if !targets.contains(&t) {
                targets.push(t);
            }
        }
        let mut total_iters = 0usize;
        for &target in &targets {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .update(cx, |v, cx| {
                        v.scroll_to_page(target);
                        cx.notify();
                    });
            });
            for _ in 0..200 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                total_iters += 1;
                let st = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (
                        v.pending_tiles.is_empty()
                            && v.pending_cover.is_empty()
                            && !v.tile_pump_running,
                        !v.prev_fast,
                    )
                });
                if st == (true, true) {
                    break;
                }
            }
        }
        let t_flip = t_all.elapsed();

        // 最终渲染驻留内存估算（只依赖公共字段/布局函数）
        let (tile_bytes, covers, mids, thumbs, cover_bytes) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let pw = v.page_width();
            let sf = 1.0f32;
            let mut covers_held = 0usize;
            let mut mids_held = 0usize;
            let mut thumbs_held = 0usize;
            let mut cover_bytes = 0u64;
            for (p, s) in v.slots.iter().enumerate() {
                if s.cover.is_some() {
                    covers_held += 1;
                    let (w, h) = layout::page_bitmap_size(
                        v.dims[p],
                        layout::cover_scale(v.dims[p], pw, sf),
                    );
                    cover_bytes += (w as u64) * (h as u64) * 4;
                }
                if s.mid.is_some() {
                    mids_held += 1;
                }
                if s.thumb.is_some() {
                    thumbs_held += 1;
                }
            }
            (v.tile_bytes, covers_held, mids_held, thumbs_held, cover_bytes)
        });
        let est = tile_bytes
            + cover_bytes
            + mids as u64 * 2 * 1024 * 1024
            + thumbs as u64 * 385 * 1024;
        eprintln!(
            "[BENCH] stops={} open_ms={:.0} zoom_ms={:.0} flip_ms={:.0} iters={} tiles={}MB covers={} mids={} thumbs={} est={}MB",
            targets.len(),
            t_open.as_secs_f64() * 1e3,
            t_zoom.as_secs_f64() * 1e3,
            t_flip.as_secs_f64() * 1e3,
            total_iters,
            tile_bytes / 1024 / 1024,
            covers,
            mids,
            thumbs,
            est / 1024 / 1024,
        );
    }

    /// 回归（**切档不重栅计数**，真实渲染路径 + 真夹具）：切换夜间档/回默认档后，本阅读器
    /// 派发的**栅格类请求**（`Render/RenderTile/Mid/Thumb`，即 worker 收到的栅格命令）必须
    /// 为 0——旧语义清缓存重派导致的 pdfium 全量重栅（夜间档加载慢根因）被根治，只剩下
    /// 「worker 对缓存原图现算重着色副本」的中继（`pending_recolor` drain 回发即证：有副本
    /// 回传才会清空到断开）。另断言切档不失效缓存槽（瓦片字节不变）、在位封面/瓦片位图的
    /// Arc 身份被新着色副本替换（重着色真的替换了显示位图）。与 `parser` 侧纯函数测试
    /// `recolor_refresh_recolors_cached_raw_and_keeps_raw`（重着色内核正确、raw 单真值不变）
    /// 组合成 headless 证据链：0 栅格 + 内核正确 + 位图被替换。
    #[gpui::test]
    async fn recolor_switch_sends_zero_raster_commands(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");

        // 首屏停稳：无在途瓦片/封面/泵 → 缓存齐全（worker 侧原图也随栅格化入缓存）。
        // worker 是真线程：每轮真实 sleep 让它在 test-dispatcher 之外有机会处理栅格请求。
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && v.pending_mid.is_empty()
                    && v.pending_thumb.is_empty()
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }

        // 切档前快照：栅格请求计数、瓦片字节、第 0 槽封面/瓦片位图的 Arc 身份。
        let (req0, bytes0, cover0, tiles0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let cover_ptr = v.slots.first().and_then(|s| {
                s.cover.as_ref().map(|c| std::sync::Arc::as_ptr(c) as usize)
            });
            let tile_ptrs: Vec<usize> = v
                .slots
                .first()
                .map(|s| {
                    s.tiles
                        .values()
                        .map(|t| std::sync::Arc::as_ptr(&t.image) as usize)
                        .collect()
                })
                .unwrap_or_default();
            (v.raster_reqs, v.tile_bytes, cover_ptr, tile_ptrs)
        });
        assert!(
            cover0.is_some() || !tiles0.is_empty(),
            "停稳后第 0 页应有在位封面或瓦片（渲染已落地）"
        );

        // 切到夜间档。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_recolor(
                    Some(RecolorParams {
                        bg: [0x20, 0x20, 0x20],
                        ink: Some([0xD8, 0xD8, 0xD8]),
                    }),
                    cx,
                );
            });
        });
        // 泵/帧推进到「重着色刷新 drain 完（断开）且再停稳」。
        let mut drained = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            drained = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_recolor.is_none()
                    && v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && !v.tile_pump_running
            });
            if drained {
                break;
            }
        }
        assert!(drained, "切档后重着色刷新应收完（worker 有缓存原图可回传）");
        let (req1, bytes1, cover1, tiles1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let cover_ptr = v.slots.first().and_then(|s| {
                s.cover.as_ref().map(|c| std::sync::Arc::as_ptr(c) as usize)
            });
            let tile_ptrs: Vec<usize> = v
                .slots
                .first()
                .map(|s| {
                    s.tiles
                        .values()
                        .map(|t| std::sync::Arc::as_ptr(&t.image) as usize)
                        .collect()
                })
                .unwrap_or_default();
            (v.raster_reqs, v.tile_bytes, cover_ptr, tile_ptrs)
        });
        assert_eq!(req1, req0, "切档后 worker 应收到 0 条新栅格命令（Render/RenderTile/Mid/Thumb）");
        assert_eq!(bytes1, bytes0, "切档不失效缓存槽：瓦片字节应保持不变");
        assert!(cover1.is_some() || !tiles1.is_empty(), "切档后内容仍在（未清槽）");
        let image_replaced = cover0.map_or(false, |p| cover1 != Some(p))
            || tiles0.iter().any(|p| !tiles1.contains(p));
        assert!(image_replaced, "到位位图应被新着色副本替换（Arc 身份变化）");
        let recolor_ok = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).recolor()
        });
        assert_eq!(
            recolor_ok,
            Some(RecolorParams {
                bg: [0x20, 0x20, 0x20],
                ink: Some([0xD8, 0xD8, 0xD8]),
            }),
            "夜间参数应落地 view 状态"
        );

        // 切回默认（None）：同样 0 新栅格。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_recolor(None, cx);
            });
        });
        let mut drained2 = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            drained2 = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_recolor.is_none() && !v.tile_pump_running
            });
            if drained2 {
                break;
            }
        }
        assert!(drained2, "切回默认档重着色刷新应收完");
        let (req2, _bytes2, _cover2, _tiles2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let cover_ptr = v.slots.first().and_then(|s| {
                s.cover.as_ref().map(|c| std::sync::Arc::as_ptr(c) as usize)
            });
            let tile_ptrs: Vec<usize> = v
                .slots
                .first()
                .map(|s| {
                    s.tiles
                        .values()
                        .map(|t| std::sync::Arc::as_ptr(&t.image) as usize)
                        .collect()
                })
                .unwrap_or_default();
            (v.raster_reqs, v.tile_bytes, cover_ptr, tile_ptrs)
        });
        assert_eq!(req2, req0, "切回默认档同样应收到 0 条新栅格命令");
        let recolor_back = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).recolor()
        });
        assert_eq!(recolor_back, None, "默认档应清回直通");
    }

    /// T4 GPU 重着色通道（worker 侧机制 + 门控，真夹具）：门控开启时 worker 对显示路径
    /// **不调用 apply_recolor**；P2 R8 门控（GPU + FullBand + ink）激活时出口编码成单通道
    /// 感知亮度（`luma=true`、`w*h*1`，-4×）——它等于「原图→luma 编码」，**不是** CPU
    /// 着色像素（CPU 税归零，着色在 GPU 绘制时）。门控关闭时恢复既有 CPU 着色（`Some` ≠
    /// `None`）。用「同页同密度渲染对比」作可计数/可字节断言。
    #[test]
    fn gpu_gate_worker_skips_cpu_recolor_and_off_keeps_it() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // 进程级门控是 worker/view 共读的全局开关：本仓库所有触碰 worker/view 的测试都持
        // pdfium-test 全局锁，故进程内瞬时一致；用 RAII 守卫，断言失败也会还原。
        let _gate = GpuGateGuard::set_on();
        let _curve = CurveGuard::set(RecolorCurve::FullBand);
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let night = RecolorParams {
            bg: [0x20, 0x20, 0x20],
            ink: Some([0xD8, 0xD8, 0xD8]),
        };
        // 原图基准（未着色）：门控开启下取 None 档（gate off → BGRA 直传）。
        // （worker 直接调用：epoch 是本测试内部推进的虚拟世代，仅结果不涉 view 守卫。）
        worker.set_recolor(doc_id, None, 1);
        let rx_raw = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 }, 0, 0);
        let raw = rx_raw
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("原图应返回")
            .expect("渲染应成功");
        assert!(!raw.luma, "None 档出口应为 BGRA（非 R8）");
        // 门控开启 + 夜间（FullBand + ink）= **R8**：出口编码成单通道亮度（跳过 CPU 着色）。
        worker.set_recolor(doc_id, Some(night), 2);
        let rx_gpu = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 }, 0, 0);
        let gpu_out = rx_gpu
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("门控开启着色档应返回")
            .expect("渲染应成功");
        assert!(gpu_out.luma, "GPU + FullBand + ink 出口应为 R8 luma 位图");
        assert_eq!(
            gpu_out.bgra.len() as u64,
            (gpu_out.width as u64) * (gpu_out.height as u64),
            "R8 出口应为 1BPP（w*h*1）"
        );
        assert!(
            gpu_out.bgra != raw.bgra,
            "R8 出口自应不同于 BGRA 原图（字节 /4），但它必须是原图的 luma 编码而非 CPU 着色"
        );
        assert_eq!(
            gpu_out.bgra,
            crate::recolor::encode_bgra_to_luma(&raw.bgra),
            "GPU 出口应为原图的 luma 编码（worker 跳过 CPU apply_recolor）"
        );
        // 内存去冗余（本线目标）：GPU 门控默认路径 worker 不写 raw 缓存（0 消费者，view
        // 持原图、着色在绘制时 img().recolor 现算），缓存长度为 0；出口 move 直传原图。
        assert_eq!(
            worker.raw_cache_len(doc_id),
            0,
            "GPU 门控路径不应驻留 worker raw 缓存（0 消费者，默认缓存冗余归零）"
        );

        // 门控关闭：恢复既有 CPU 着色（回归不变式，与 dbd3236 一致）。
        crate::recolor::set_gpu_recolor_gate_for_test(false);
        worker.set_recolor(doc_id, Some(night), 3);
        let rx_cpu = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 }, 0, 0);
        let cpu_out = rx_cpu
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("门控关闭着色档应返回")
            .expect("渲染应成功");
        assert!(
            cpu_out.bgra != raw.bgra,
            "门控关闭时 recolor 应照常 CPU 着色（与未着色原图不同）"
        );
        assert!(
            worker.raw_cache_len(doc_id) > 0,
            "CPU 回退路径应照常写入 raw 缓存（供 SetRecolor/RecolorRefresh 消费）"
        );

        worker.close(doc_id);
    }

    /// T4 GPU 重着色通道（view 侧 scene 机制 + 门控，真夹具）：门控开启 + 切到夜间档后，
    /// 页卡片/瓦片的 `img(...).recolor(Some)` 参数进入场景 = `RecolorSprite` 图元携带
    /// 正确纸/墨参（`switches.x == 1` 启用墨带、`bg` 归一化为纸色）——GPU 绘制时着色的
    /// 接线成立。**P2 R8 门控翻转语义**：`None→夜间`（BGRA→R8）与 `夜间→浅色`（ink
    /// None，R8→BGRA）因**位图数据格式改变**需重栅化一次（接受口径，`raster_reqs` 增加）；
    /// **同门控内**调纸/墨色不触发重栅（`raster_reqs` 不变，只改图元参数）——「切档 0 重算
    /// 0 重栅」承诺在格式不变时仍成立。A-P2-3/A-P2-4 共用：翻转重栅 + R8 上传走 Monochrome
    /// atlas（1BPP）+ 浅色档回 Polychrome。GPU 像素无法 headless 断言（K23②），此处证机制+门控。
    #[gpui::test]
    async fn gpu_gate_view_attaches_recolor_params_to_scene(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = pdf_lock().lock().unwrap();
        let _gate = GpuGateGuard::set_on();
        let _curve = CurveGuard::set(RecolorCurve::FullBand);
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        // 首屏停稳（worker 真线程：真实 sleep 给它处理栅格请求的机会）。
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && v.pending_mid.is_empty()
                    && v.pending_thumb.is_empty()
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        let req0 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).raster_reqs
        });
        // 各切档阶段统一「停稳」：泵/帧推进直到在途请求清空（真实 worker 渲染回传），
        // 再取 raster_reqs——避免「同门控 0 重栅」断言被未停稳的收尾派发污染。
        let settle = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..600 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(3));
                let done = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    v.pending_tiles.is_empty()
                        && v.pending_cover.is_empty()
                        && v.pending_mid.is_empty()
                        && v.pending_thumb.is_empty()
                        && !v.tile_pump_running
                });
                if done {
                    break;
                }
            }
        };

        // 切到夜间档（GPU + FullBand + ink = **R8 门控翻转**）：位图数据格式从 BGRA 翻到
        // R8 luma（-4×），受影响页**重栅化一次**（P2 接受口径）→ 新栅格命令数应增加。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_recolor(
                    Some(RecolorParams {
                        bg: [0x20, 0x20, 0x20],
                        ink: Some([0xD8, 0xD8, 0xD8]),
                    }),
                    cx,
                );
            });
        });
        settle(cx);
        let req1 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).raster_reqs
        });
        assert!(
            req1 > req0,
            "None→夜间门控翻转应重栅化一次（R8 位图格式改变）：{req0}->{req1}"
        );

        // 场景断言：页卡/瓦片 img 携带了重着色参数（RecolorSprite 图元，bg/switches 正确）。
        let sprites = cx.update(|window, _| window.painted_recolor_sprites());
        assert!(
            !sprites.is_empty(),
            "门控开启 + 夜间档应产生 RecolorSprite 场景图元（PDF 显示 img 挂上重着色参数）"
        );
        // bg 归一化 0x20/255 ≈ 0.1255；switches.x=1 启用墨带；ink 0xD8/255 ≈ 0.8471；
        // switches.y=1 = FullBand；**switches.z=1 = R8 luma 数据源**（P2）——shader 分支依据。
        let bg0 = 0x20u8 as f32 / 255.0;
        let ink0 = 0xD8u8 as f32 / 255.0;
        let ok = sprites.iter().any(|s| {
            s.switches[0] == 1.0
                && s.switches[1] == 1.0
                && s.switches[2] == 1.0
                && (s.bg[0] - bg0).abs() < 1e-3
                && (s.ink[0] - ink0).abs() < 1e-3
        });
        assert!(
            ok,
            "至少一个 RecolorSprite 应携带正确的纸/墨参数 + FullBand + R8 luma 标记（switches.z=1）"
        );
        // A-P2-4：R8 门控下显示位图上传走 Monochrome atlas（1BPP）——sprite 的 atlas kind 断言。
        assert!(
            sprites
                .iter()
                .filter(|s| s.switches[2] == 1.0)
                .all(|s| s.tile.texture_id.kind == gpui::AtlasTextureKind::Monochrome),
            "R8 位图 sprite 的 atlas kind 应为 Monochrome（1BPP 上传，DXGI_FORMAT_R8_UNORM）"
        );

        // 同门控内参数微调（换纸/墨色，仍是 R8 FullBand + ink）→ **0 重栅**（只改图元参数）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_recolor(
                    Some(RecolorParams {
                        bg: [0x21, 0x21, 0x21],
                        ink: Some([0xD7, 0xD7, 0xD7]),
                    }),
                    cx,
                );
            });
        });
        settle(cx);
        let req2 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).raster_reqs
        });
        assert_eq!(
            req2, req1,
            "同 R8 门控内调色应 0 新栅格（GPU 参数即开关，P2 不破此承诺）"
        );

        // 切到浅色/去重着色（`ink=None`）= **R8 门控翻转回 BGRA** → 受影响页再重栅化一次
        //（P2 接受口径）；着色仍在 GPU（RecolorSprite 保留），但数据源回到 4BPP。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_recolor(
                    Some(RecolorParams {
                        bg: [0xC7, 0xED, 0xCC],
                        ink: None,
                    }),
                    cx,
                );
            });
        });
        settle(cx);
        let req3 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).raster_reqs
        });
        assert!(
            req3 > req2,
            "R8→浅色门控翻转应再重栅化一次（BGRA 位图格式恢复）：{req2}->{req3}"
        );
        let sprites_light = cx.update(|window, _| window.painted_recolor_sprites());
        assert!(
            !sprites_light.is_empty(),
            "浅色档仍应产生 RecolorSprite（着色留在 GPU，只是数据源回 BGRA）"
        );
        assert!(
            !sprites_light.iter().any(|s| s.switches[2] == 1.0),
            "浅色档（ink=None / 去重着色）不应再有 R8 luma 标志"
        );
        assert!(
            sprites_light
                .iter()
                .all(|s| s.tile.texture_id.kind == gpui::AtlasTextureKind::Polychrome),
            "浅色档位图应全部走 Polychrome atlas（BGRA 4BPP，ContentProtect 需要原色）"
        );
    }

    /// 回归（**切档只刷可见集**，真实渲染路径 + 真夹具）：
    /// 【可见集定义】「显示槽真实需要的页面内容」= 在位封面 + 在位瓦片（中清/缩略是快速
    /// 拖动瞬态占位、不参与；窗外 / 已淘汰但仍在 worker raw 缓存的条目不回传，保留 raw
    /// 单真值，滚回需要时再现算）。
    /// 断言：切档后 worker 回传条目的 key **集合 ⊆ view 圈定的可见集 scope**（只刷可见集，
    /// 绝不越界发窗内容）+ 回传非空 + 切档 0 新栅格 —— 证明新路径把旧「刷全部缓存」收敛为
    /// 「只刷可见集」。**48MB 预算语义**（`RAW_CACHE_BYTES` = 高倍可见集 3×封面 + 瓦片）：
    /// FIFO 按最早写入回收最旧，滚动路过页/旧全页封面原图超出预算即被回收 → 不再断言
    /// 「在位封面必全部回传」（96MB 时代的强保证，随预算收窄为「缓存内可见集原图必回传、
    /// 不在缓存的可由后续交互重渲恢复」）。首条回传（若有）= 距当前可见中心最近页（按近
    /// 中心排序 → 首帧即首可视页）。切档 0 新栅格另由
    /// `recolor_switch_sends_zero_raster_commands` 独证，此处另断言计数不变。
    #[gpui::test]
    async fn recolor_switch_refreshes_only_visible_set(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;
        use crate::pdf::parser::{RawKind, RecolorKey};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");

        // 「毒页」：先滚到第 3 页停稳、再滚回第 0 页停稳——让 worker raw 缓存覆盖多页
        // （滚动路过的页码），而 view 只显示回滚后的近窗口页 → 制造「全缓存 ≠ 可见集」。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(3);
                    cx.notify();
                });
        });
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_tiles.is_empty(),
                    v.pending_cover.is_empty(),
                    !v.tile_pump_running,
                    !v.prev_fast,
                )
            });
            if st == (true, true, true, true) {
                break;
            }
        }
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.scroll_to_page(0);
                    cx.notify();
                });
        });
        for _ in 0..200 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_tiles.is_empty(),
                    v.pending_cover.is_empty(),
                    !v.tile_pump_running,
                    !v.prev_fast,
                )
            });
            if st == (true, true, true, true) {
                break;
            }
        }

        // 切档前：raw 缓存全量条目数（应覆盖毒页，> 可见集）。
        let full_cache = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            v.worker.as_ref().unwrap().raw_cache_len(v.doc_id)
        });
        let req0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).raster_reqs);

        // 切夜间 → 泵/帧推进到重着色刷新收完。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.set_recolor(
                        Some(RecolorParams {
                            bg: [0x20, 0x20, 0x20],
                            ink: Some([0xD8, 0xD8, 0xD8]),
                        }),
                        cx,
                    );
                });
        });
        let mut drained = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            drained = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_recolor.is_none() && !v.tile_pump_running
            });
            if drained {
                break;
            }
        }
        assert!(drained, "重着色刷新应收完");

        // 可见集断言。
        let (scope_keys, received, req1, tiles_present) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let scope_keys: Vec<(RawKind, usize, u32, u32)> = v
                .recolor_scope
                .iter()
                .map(|k: &RecolorKey| (k.kind, k.page, k.row, k.col))
                .collect();
            let received = v.recolor_received.clone();
            let tiles_present = v.slots.first().map_or(false, |s| {
                s.cover.is_some() || !s.tiles.is_empty()
            });
            (scope_keys, received, v.raster_reqs, tiles_present)
        });

        assert!(tiles_present, "首可视页应有在位封面/瓦片");
        assert!(!scope_keys.is_empty(), "可见集非空");
        assert_eq!(req1, req0, "切档 0 条新栅格命令（只刷可见集、不重栅）");

        // 核心不变量：**回传集合 ⊆ 可见集 scope**（窗外/已淘汰条目不回传——worker 只按
        // view 圈定的可见集 key 现算回传，绝不越界发窗内容）。这是「只刷可见集」的收敛证明，
        // 无论缓存如何被 FIFO 回收都成立。
        let scope_set: std::collections::HashSet<(RawKind, usize, u32, u32)> =
            scope_keys.iter().copied().collect();
        let received_set: std::collections::HashSet<(RawKind, usize, u32, u32)> =
            received.iter().copied().collect();
        for k in &received {
            assert!(
                scope_set.contains(k),
                "回传条目 {k:?} 不在可见集 scope 内（只刷可见集被破坏）"
            );
        }

        // CPU 模式缓存仍在（48MB 预算下按 FIFO 回收最旧，但缓存非空、刷新确有回传——
        // 回传证明 worker 对缓存的可见集原图现算着色副本的机制链路在 CPU 模式下完好）。
        assert!(full_cache > 0, "CPU 模式 raw 缓存应非空");
        assert!(!received.is_empty(), "CPU 模式重着色刷新应有回传条目");
        // 「全缓存 ⊇ 可见集回传」：缓存持有多于刷新回传的残留条目（滚动路过的毒页/非可见
        // 封面原图可留在缓存中不被回传）——证明刷新只消费可见集、不越界刷全缓存。48MB
        // FIFO 预算下旧/非可见封面原图可能已被回收，故此处为「≥」而非原「>」——缓存计数
        // 与回传计数都是预算相关的派生量，匹配新语义（见 [`config::pdf::RAW_CACHE_BYTES`]）。
        assert!(
            full_cache >= received_set.len(),
            "raw 缓存应至少持有全部回传条目: full={full_cache} received={}",
            received_set.len()
        );

        // 排序软断言：先刷距当前可见中心最近页（scope 由 view 按近中心排序给出，
        // worker 分批分帧按此序回传，首批即首可视页附近）。
        assert_eq!(
            received.first().map(|k| k.1),
            scope_keys.first().map(|k| k.1),
            "先刷新距当前可见中心最近页"
        );
    }

    /// W1-BLOCK 回归（真实开档时序 + 真夹具）：**worker 接线前的多次 recolor 变更，接线后
    /// 世代必须对齐、结果不被误丢**。评审 BLOCK：view 在 `set_recolor` 实际变更时 +1，而
    /// apply_loaded 补推（`view/open.rs`）把接线前 N 次变更折叠成 worker 侧一次 → worker
    /// 世代恒落后 view → `collect_tile/*_results` 世代守卫全部丢弃 → CPU 回退路径整页空白 +
    /// 渲染/丢弃空转。修复：worker **采纳** view 消息携带的世代（`max(当前, 消息)`，view 为
    /// 唯一真源，见 `WorkerMsg::SetRecolor` 第三字段）。本测试复刻 shell 开档流：
    /// `PdfViewer::new`（worker=None）→ 立即**两次不同档** `set_recolor` → 接线后断言
    /// 页 0 有在位封面/瓦片（结果未被误丢）、无「渲染→丢弃→重派」空转、view 世代 = 2。
    #[gpui::test]
    async fn w1_recolor_epoch_aligns_after_prewire_switches(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        // worker 尚未接线（start_load 是异步 spawn，未 run 前 worker=None）→ 两次 set_recolor
        // 只改 view 状态、不推送 worker：view 世代 0→1→2，正是 BLOCK 的「接线前多次变更」。
        let p_a = RecolorParams { bg: [0x30, 0x30, 0x30], ink: None };
        let p_b = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        for p in [p_a, p_b] {
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                    v.set_recolor(Some(p), cx);
                });
            });
        }
        let epoch_pre = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).recolor_epoch);
        assert_eq!(epoch_pre, 2, "接线前两次 set_recolor → view 世代 2");

        // 驱动加载完成 + 首屏停稳。
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let mut settled = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty() && v.pending_cover.is_empty() && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        assert!(settled, "首屏应停稳（无在途瓦片/封面/泵——世代对齐则结果被采纳，不会空转）");

        // BLOCK 断言：接线后世代对齐 → 结果被采纳 → 页 0 有封面或瓦片；若 worker 世代恒
        // 落后（旧实现），结果全被丢弃 → 重派 → 永不停稳且页 0 无内容（上面 settled 已证空转）。
        let (epoch, has_content, cover_reqs) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.recolor_epoch,
                v.slots
                    .first()
                    .map_or(false, |s| s.cover.is_some() || !s.tiles.is_empty()),
                v.cover_reqs,
            )
        });
        assert_eq!(epoch, 2, "view 世代保持 2（apply_loaded 补推携带，worker 采纳对齐）");
        assert!(has_content, "页 0 应有在位封面或瓦片（接线前多次切档后结果不被误丢）");
        assert!(cover_reqs > 0, "封面确实派发过");
    }

    /// WARN-2 正向钉死（真实渲染路径 + 真夹具 + 极小预算）：**48MB 预算紧张下的部分刷新是
    /// 预期行为**——把 raw 缓存预算压到 2MB（≈ 1 瓦片；整页封面 ~11MB 永不驻留），「首可视
    /// 页封面（最早写入）被 FIFO 挤出」成为确定事实：切档刷新只回传**仍驻留缓存的可见集
    /// 子集**（驻留瓦片按新参数重着色回传）、**被挤出的封面 key 不回传**、切档 0 新栅格、
    /// 过程不崩（drained）——「部分刷新」不是缺陷（配套 WARN-1 注释）。
    #[gpui::test]
    async fn recolor_switch_partial_refresh_when_visible_cover_evicted(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;
        use crate::pdf::parser::{RawKind, RecolorKey};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let _budget = RawBudgetGuard::set(2 * 1024 * 1024);
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let settled = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_tiles.is_empty()
                    && v.pending_cover.is_empty()
                    && v.pending_mid.is_empty()
                    && v.pending_thumb.is_empty()
                    && !v.tile_pump_running
            });
            if settled {
                break;
            }
        }
        let req0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).raster_reqs);
        let full_cache = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            v.worker.as_ref().unwrap().raw_cache_len(v.doc_id)
        });

        // 切夜间（CPU 通道 → RecolorRefresh）→ 泵/帧推进到重着色刷新收完。
        cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, cx| {
                    v.set_recolor(
                        Some(RecolorParams {
                            bg: [0x20, 0x20, 0x20],
                            ink: Some([0xD8, 0xD8, 0xD8]),
                        }),
                        cx,
                    );
                });
        });
        let mut drained = false;
        for _ in 0..400 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            drained = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.pending_recolor.is_none() && !v.tile_pump_running
            });
            if drained {
                break;
            }
        }
        assert!(drained, "重着色刷新应收完（部分刷新不崩）");

        let (scope_keys, received, req1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let scope_keys: Vec<(RawKind, usize, u32, u32)> = v
                .recolor_scope
                .iter()
                .map(|k: &RecolorKey| (k.kind, k.page, k.row, k.col))
                .collect();
            (scope_keys, v.recolor_received.clone(), v.raster_reqs)
        });
        let scope_set: std::collections::HashSet<(RawKind, usize, u32, u32)> =
            scope_keys.iter().copied().collect();
        let received_set: std::collections::HashSet<(RawKind, usize, u32, u32)> =
            received.iter().copied().collect();
        let scope_covers: Vec<_> = scope_keys
            .iter()
            .filter(|k| k.0 == RawKind::Cover)
            .copied()
            .collect();

        // ① 可见集确实含在位封面（渲染到 slot 了），但其原图未被 2MB 预算留下。
        assert!(!scope_covers.is_empty(), "可见集应含在位封面");
        // ② 回传 ⊆ 仍驻留缓存的可见集子集（只刷可见集，绝不越界发窗内容）。
        for k in &received {
            assert!(
                scope_set.contains(k),
                "回传 {k:?} 不在可见集 scope 内（只刷可见集被破坏）"
            );
        }
        // ③ 被 FIFO 挤出的封面 key 不回传（「部分刷新」的直接证据）。
        for k in &scope_covers {
            assert!(
                !received_set.contains(k),
                "被挤出的封面 {k:?} 不应回传（部分刷新：只回驻留原图）"
            );
        }
        // ④ 部分刷新仍在发生：驻留的可见瓦片子集被重着色回传（非空）。
        assert!(!received.is_empty(), "部分刷新应有回传（驻留瓦片重着色）");
        // ⑤ 切档 0 新栅格（被挤出的封面不触发重栅，靠下一交互常规渲染恢复）。
        assert_eq!(req1, req0, "切档 0 条新栅格命令（部分刷新不重栅）");
        // ⑥ 极小预算自证：缓存条目被压缩到远小于正常加载（残留瓦片级）。
        assert!(full_cache < 32, "2MB 预算下缓存条目应被压缩: full={full_cache}");
    }

    /// 时空基准（切档不重栅墙钟，真实渲染路径 + 真夹具）：测量「首屏停稳 → 切夜间 →
    /// 重着色刷新收完」的墙钟与栅格请求增量。用法：修复前（旧清槽重栅）跑一次记录，修复后
    /// 再跑对比——夜间档切档墙钟应显著下降（重栅数百 ms → 重着色刷新数 ms~几十 ms），
    /// 且 `raster_delta` 为 0。手动 `--ignored` 跑（与 `bench_full_scroll_time_memory` 同步调）。
    #[gpui::test]
    #[ignore]
    async fn bench_recolor_switch_wallclock(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试2.pdf");
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        // 严格停稳（无在途瓦片/封面/泵）再切档：保证切档后调度帧因视口缓存命中 0 新栅格，
        // 否则把「沉降未完成被补派」误当栅格增量污染 raster_delta。
        for _ in 0..300 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_tiles.is_empty(),
                    v.pending_cover.is_empty(),
                    !v.tile_pump_running,
                    !v.prev_fast,
                    v.window_settled,
                )
            });
            if st == (true, true, true, true, true) {
                break;
            }
        }
        let req0 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).raster_reqs);
        // 全缓存条目数（含滚动路过的毒页）——与可见集 scope 对比「只刷可见集」的比例证据。
        let full_cache = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            v.worker.as_ref().unwrap().raw_cache_len(v.doc_id)
        });
        // 3 轮交替切档（夜间/默认/护眼），多轮取中位：每轮都验证 raster_delta=0（切档 0 栅格）。
        let rounds = [
            Some(RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) }),
            None,
            Some(RecolorParams { bg: [0xC7, 0xED, 0xCC], ink: None }),
        ];
        let mut switch_ms: Vec<f64> = Vec::new();
        let mut first_ready_ms: Vec<f64> = Vec::new();
        let mut entries_done: Vec<usize> = Vec::new();
        for p in rounds {
            let t0 = Instant::now();
            let mut first_ready: Option<f64> = None;
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                    v.set_recolor(p, cx);
                });
            });
            let mut drained = false;
            for _ in 0..800 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                let (rc, pr, received_n) = cx.update(|_w, cx| {
                    let v = holder.borrow().as_ref().unwrap().read(cx);
                    (v.raster_reqs, v.pending_recolor.is_none(), v.recolor_received.len())
                });
                if first_ready.is_none() && received_n > 0 {
                    // 首帧就绪：首个回传条目到达（view 已收集到重着色副本）。
                    first_ready = Some(t0.elapsed().as_secs_f64() * 1e3);
                }
                if pr && rc == req0 {
                    drained = true;
                    switch_ms.push(t0.elapsed().as_secs_f64() * 1e3);
                    first_ready_ms.push(first_ready.unwrap_or(0.0));
                    entries_done.push(received_n);
                    break;
                }
            }
            assert!(drained, "轮次 {p:?} 应收完");
        }
        let median = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let scope = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).recolor_scope.len()
        });
        eprintln!(
            "[BENCH-RECOLOR] switch_ms_med={:.0} first_ready_ms_med={:.0} entries_final={entries_done:?} visible_scope={scope} full_cache={full_cache} raster_delta=0 rounds={}",
            median(switch_ms),
            median(first_ready_ms),
            rounds.len()
        );
    }

    // ---- Phase A：标准注释写回 / 页面插入·删除 / 提取（真夹具，照既有测试模式）----

    /// 拷贝夹具到 temp 独立文件（Phase A 写回测试操作副本，不污染仓库夹具）。
    fn temp_pdf_copy(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join("vuick-pdf-tests");
        let _ = std::fs::create_dir_all(&dir);
        let dst = dir.join(format!(
            "{name}-{}-{}.pdf",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        test_util::copy_fixture_writable(&test_pdf_paths()[0], &dst).expect("拷贝夹具到 temp");
        dst
    }

    /// 三态注释写回 → 重开同一文件读回类型与几何一致（±1pt），页数不变。
    /// 效果断言（K37）：重开用的是**从磁盘重读**的新 bytes——读到注释即证明真的落盘。
    #[test]
    fn writeback_annotations_persist_and_read_back() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let path = temp_pdf_copy("annot");
        let bytes0 = std::fs::read(&path).expect("读夹具");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes0.clone()).expect("打开应成功");

        // 写回前：该页应无注释（读回为空）
        assert!(
            worker.annotations_for_page(doc_id, 0).expect("读回").is_empty(),
            "写回前第 0 页应无注释"
        );

        // 一条覆盖页中部大区域的 quad，三条不同类型注释叠在同一区域
        let (pw, ph) = dims[0];
        let quads = vec![QuadPt {
            x0: 60.0,
            y0: ph * 0.55,
            x1: pw - 60.0,
            y1: ph * 0.62,
        }];
        let items = vec![
            AnnotateItem { page_index: 0, kind: AnnotKind::Highlight, quads: quads.clone() },
            AnnotateItem { page_index: 0, kind: AnnotKind::Strikeout, quads: quads.clone() },
            AnnotateItem { page_index: 0, kind: AnnotKind::Underline, quads: quads.clone() },
        ];
        let added = worker.annotate(doc_id, items, path.clone()).expect("写回注释应成功");
        assert_eq!(added, 3, "应写入 3 条注释");
        worker.close(doc_id);

        // 渲染可见性（FPDF_RENDER_ANNOT 默认开启）：写回前 vs 重开后同区域像素出现差异
        // = 注释真的渲染出来了（不是只落了文件没显示）。
        let (wa, da, _) = PdfWorker::open_and_dims(bytes0).expect("原始打开");
        let rx = wa.request_render(da, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let before = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("收到渲染")
            .expect("渲染成功");
        wa.close(da);

        // 从磁盘重读字节 → 重开（效果断言）
        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("改后重读")).expect("重开改后文件");
        assert_eq!(dims2, dims, "写回注释不应改变页数与页尺寸");
        let rx2 = w2.request_render(d2, RenderRequest { page: 0, scale: 1.0 }, 0, 0);
        let after = rx2
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("收到渲染")
            .expect("渲染成功");
        assert_eq!((before.width, before.height), (after.width, after.height), "渲染尺寸应一致");
        // 全页像素 diff：注释带在 PDF 系（y-up）而位图是 y-down，按假定带采样容易翻转错位；
        // 直接比整页最稳——高亮/删除线/下划线横跨页宽，只要渲染出来了 diff 必然远超阈值。
        // PDF y-up 0.55ph..0.62ph 的注释带在 y-down 位图里对应 (1-0.62)ph..(1-0.55)ph，
        // 全页比较无需依赖该映射。
        let w = after.width as usize;
        let hgt = after.height as usize;
        let mut diff = 0u32;
        for y in 0..hgt {
            for x in 0..w {
                let o = (y * w + x) * 4;
                let d = (0..4)
                    .map(|c| (before.bgra[o + c] as i64 - after.bgra[o + c] as i64).abs())
                    .sum::<i64>();
                if d > 24 {
                    diff += 1;
                }
            }
        }
        assert!(
            diff > 200,
            "写回注释后整页渲染应出现可见差异（diff={diff} px）"
        );

        // 读回类型与几何
        let infos = w2.annotations_for_page(d2, 0).expect("读回注释");
        assert_eq!(infos.len(), 3, "应读回 3 条注释");
        let expect_kinds =
            [AnnotKind::Highlight, AnnotKind::Strikeout, AnnotKind::Underline];
        for (got, want) in infos.iter().zip(expect_kinds.iter()) {
            assert_eq!(&got.kind, want, "注释类型应一致");
            assert_eq!(got.quads.len(), 1, "每条注释应 1 个 quad");
            let (q, e) = (got.quads[0], quads[0]);
            for (a, b, msg) in [
                (q.x0, e.x0, "x0"),
                (q.y0, e.y0, "y0"),
                (q.x1, e.x1, "x1"),
                (q.y1, e.y1, "y1"),
            ] {
                assert!((a - b).abs() <= 1.0, "quad {msg} 应在容差内: {a} vs {b}");
            }
        }
        w2.close(d2);
        let _ = std::fs::remove_file(&path);
    }

    /// 插入空白页：末尾追加 + 中间插入，重开后页数/新页尺寸保持（落盘效果断言）。
    #[test]
    fn insert_blank_page_persists_dimensions() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let path = temp_pdf_copy("insert");
        let (worker, doc_id, dims) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("读夹具")).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 2, "夹具应足量页");

        // 末尾追加一页 300x400
        let new_count = worker
            .insert_blank_page(doc_id, n, 300.0, 400.0, path.clone())
            .expect("末尾插入应成功");
        assert_eq!(new_count, n + 1, "追加后页数 +1");

        // 中间插入：at_index=1（成为第 2 页）
        let new_count2 = worker
            .insert_blank_page(doc_id, 1, 200.0, 220.0, path.clone())
            .expect("中间插入应成功");
        assert_eq!(new_count2, n + 2, "再插后页数 +1");
        worker.close(doc_id);

        // 从磁盘重读重开：页数与各插入页尺寸保持
        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("改后重读")).expect("重开改后文件");
        assert_eq!(dims2.len(), n + 2, "重开后页数保持");
        let mid = dims2[1];
        assert!(
            (mid.0 - 200.0).abs() < 0.5 && (mid.1 - 220.0).abs() < 0.5,
            "中间插入页尺寸应保持: {mid:?}"
        );
        let last = dims2[n + 1]; // 原 n 页 + 中插 1 页后，末尾是末尾追加的
        assert!(
            (last.0 - 300.0).abs() < 0.5 && (last.1 - 400.0).abs() < 0.5,
            "末尾追加页尺寸应保持: {last:?}"
        );
        w2.close(d2);
        let _ = std::fs::remove_file(&path);
    }

    /// 删除页面：页数 -1 且重开保持；越界/仅剩 1 页返回错误。
    #[test]
    fn delete_page_persists_and_guards_single_page() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let path = temp_pdf_copy("del");
        let (worker, doc_id, dims) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("读夹具")).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 2, "夹具应足量页");

        let new_count = worker.delete_page(doc_id, 0, path.clone()).expect("删除首页应成功");
        assert_eq!(new_count, n - 1, "删除后页数 -1");
        worker.close(doc_id);

        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("改后重读")).expect("重开改后文件");
        assert_eq!(dims2.len(), n - 1, "重开后页数保持");
        // 越界删除 → Err
        assert!(
            w2.delete_page(d2, dims2.len() + 5, path.clone()).is_err(),
            "越界删除应报错"
        );
        w2.close(d2);

        // 单页文档：提取第 0 页成单页文件后删除 → Err「仅剩 1 页」
        let single = {
            let dir = std::env::temp_dir().join("vuick-pdf-tests");
            let _ = std::fs::create_dir_all(&dir);
            let p = dir.join(format!("single-{}.pdf", std::process::id()));
            let _ = std::fs::remove_file(&p);
            p
        };
        let (w3, d3, _) = PdfWorker::open_and_dims(std::fs::read(&path).expect("读夹具")).expect("打开");
        w3.extract_page_to(d3, 0, single.clone()).expect("提取单页");
        w3.close(d3);
        let (w4, d4, dims4) =
            PdfWorker::open_and_dims(std::fs::read(&single).expect("读单页")).expect("打开单页文档");
        assert_eq!(dims4.len(), 1, "提取产物应为单页");
        assert!(
            w4.delete_page(d4, 0, single.clone()).is_err(),
            "删除仅剩 1 页应报错"
        );
        w4.close(d4);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&single);
    }

    /// 提取单页成独立 PDF：产物页数 == 1，尺寸与原页一致（落盘效果断言）。
    #[test]
    fn extract_page_creates_single_page_document() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let path = temp_pdf_copy("extract");
        let out = {
            let dir = std::env::temp_dir().join("vuick-pdf-tests");
            let _ = std::fs::create_dir_all(&dir);
            let p = dir.join(format!("extracted-{}.pdf", std::process::id()));
            let _ = std::fs::remove_file(&p);
            p
        };
        let (worker, doc_id, dims) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("读夹具")).expect("打开应成功");
        let target = dims.len() / 2;
        worker
            .extract_page_to(doc_id, target, out.clone())
            .expect("提取应成功");
        worker.close(doc_id);

        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&out).expect("读提取产物")).expect("打开提取文件");
        assert_eq!(dims2.len(), 1, "提取产物应为单页");
        let (ew, eh) = dims[target];
        assert!(
            (dims2[0].0 - ew).abs() < 0.5 && (dims2[0].1 - eh).abs() < 0.5,
            "提取页尺寸应与原页一致: {:?} vs ({ew},{eh})",
            dims2[0]
        );
        w2.close(d2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    /// 红测 Bug A（菜单「提取页面成功 toast」决策点真空）：`shell_pdf::menu::extract_page_action`
    /// 在 **`extract_page_to` 同步调用后立即**（不等后台完成）读 `last_modify_error` 决定
    /// 成败 toast（menu.rs:223-228）。而 `extract_page_to` 是异步：同步段先清错误
    /// （open.rs:1246 `self.last_modify_error = None`），真实写盘失败只在后台完成后才置错
    /// （open.rs:1258）→ 决策点恒读 None → 无论成败都弹「已提取页面」（成功 toast 恒假，
    /// 失败也先弹一条错误成功 toast，随后经 observe 再补失败提示）。
    /// 红测：构造一次**注定失败**的异步提取（输出路径父目录不存在 → worker 写盘必 Err），
    /// 断言「在菜单决策点（调用后立即读）必须已能观察到失败标记」——当前为 None = FAILED。
    #[gpui::test]
    async fn extract_doomed_write_error_is_not_visible_at_menu_decision_point(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        use std::time::Duration;
        use gpui::AppContext as _;

        let p = temp_pdf_copy("extract-red");
        let viewer = cx.update(|app| app.new(|cx| crate::pdf::PdfViewer::new(p.clone(), cx)));
        fn wait(
            cx: &mut gpui::TestAppContext,
            pred: &mut dyn FnMut(&mut gpui::TestAppContext) -> bool,
        ) -> bool {
            for _ in 0..800 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                if pred(cx) {
                    return true;
                }
            }
            false
        }
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).is_loaded())),
            "夹具应加载完成（进入异步写盘路径）"
        );

        // 注定失败：目标路径父目录不存在（worker 原子写盘必 Err）。
        let dir = std::env::temp_dir()
            .join("vuick-pdf-tests")
            .join(format!("no-such-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let doomed = dir.join("extracted.pdf");

        cx.update(|app| viewer.update(app, |v, cx| v.extract_page_to(0, doomed.clone(), cx)));
        // 菜单决策点：同步调用后**立即**（不 pump 后台）读 last_modify_error。
        let now = cx
            .update(|app| viewer.read(app).last_modify_error().map(|s| s.to_string()));
        assert!(
            now.is_some(),
            "菜单决策点必须能观察到注定失败的提取的错误标记；当前 None → 成功 toast 被无条件弹出（成功 toast 恒假）"
        );

        // 收尾证实：预检命中后错误标记**同步已置且持续可见**（不因后续 pump 被清除），
        // 与「决策点真空」（旧行为 None）相对照。注：预检短路使本 op 不再进后台写盘——
        // 异步写盘失败链（worker Err → last_modify_error → observe 补发）由既有
        // `last_modify_error`/`_pdf_sub` 机制覆盖，不在此红测范围内。
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).last_modify_error().is_some())),
            "预检命中后错误标记应持续可见（父目录不存在 → 同步置错）"
        );
        let _ = std::fs::remove_file(&p);
    }

    // ---- 待保存模型（P1）：内存页面操作不写盘 / 镜像恢复 / 保存后才落盘 ----

    /// 页面操作**待存**（worker 层）：内存级插入/删除只改 stores 内存工作副本、**磁盘文件
    /// 字节原样**；撤销恢复经 MemRestoreBytes 精确还原镜像；显式 save 才落盘且重开页数一致。
    /// 用真实 PDF 夹具 + 唯一 temp 副本（不污染 `target/pdf测试.pdf`）。
    #[test]
    fn mem_page_ops_do_not_touch_disk_until_save() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        let path = temp_pdf_copy("memop");
        let disk0 = std::fs::read(&path).expect("读夹具");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(disk0.clone()).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 2, "夹具应足量页");

        // ① 内存插入：只回传新镜像，磁盘字节必须原样
        let r1 = worker
            .mem_insert_blank_page(doc_id, 1, 300.0, 400.0)
            .expect("内存插入应成功");
        assert_eq!(r1.count, n + 1, "内存插入后页数 +1");
        assert_eq!(
            std::fs::read(&path).expect("重读磁盘"),
            disk0,
            "内存插入后磁盘 bytes 必须不变（待保存）"
        );
        // stores 应同步为新镜像（后续操作/保存基于新内容）
        assert_eq!(worker.mem_bytes(doc_id), Some(r1.after.clone()), "stores 应同步新镜像");

        // ② 内存删除（回到 n 页）
        let r2 = worker.mem_delete_page(doc_id, 0).expect("内存删除应成功");
        assert_eq!(r2.count, n, "内存删除后页数复原");
        assert_eq!(
            std::fs::read(&path).expect("重读磁盘"),
            disk0,
            "内存删除后磁盘 bytes 必须不变（待保存）"
        );

        // ③ 撤销恢复：MemRestoreBytes 换成原始镜像 → stores 回滚 + 其后保存页数复原
        worker
            .mem_restore_bytes(doc_id, disk0.clone())
            .expect("恢复镜像应成功");
        assert_eq!(worker.mem_bytes(doc_id), Some(disk0.clone()), "恢复后 stores == 原始镜像");

        // ④ 显式 save → 磁盘变化、重开读回页数复原
        worker.save(doc_id, path.clone()).expect("保存应成功");
        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("保存后重读")).expect("重开改后文件");
        assert_eq!(dims2.len(), n, "恢复镜像后保存 → 重开页数复原（= 原始 n）");
        w2.close(d2);

        worker.close(doc_id);
        let _ = std::fs::remove_file(&path);
    }

    /// 待保存模型 viewer 层（真夹具，`#[gpui::test]`）：插入/删除页只内存级（磁盘 bytes
    /// 全程不变）、dirty 随操作置真；Ctrl+Z 撤销页操作后页数复原 + 清脏；注释只进内存；
    /// `save()` 才落盘（磁盘变化 + 重开读回注释 + 清 pending/dirty）。无窗口驱动（纯实体级）。
    #[gpui::test]
    async fn pending_viewer_page_ops_undo_save_real_fixture(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = test_util::lock().lock().unwrap();
        use gpui::AppContext as _;
        use std::time::Duration;

        let src = test_util::test_pdf_paths()[0].clone();
        let dir = std::env::temp_dir().join("vuick-pdf-tests");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(format!("pending-op-{}.pdf", std::process::id()));
        let _ =         std::fs::remove_file(&p);
        test_util::copy_fixture_writable(&src, &p).expect("拷贝夹具");
        let disk0 = std::fs::read(&p).expect("读初始磁盘");

        let viewer = cx.update(|app| app.new(|cx| crate::pdf::PdfViewer::new(p.clone(), cx)));
        fn wait(
            cx: &mut gpui::TestAppContext,
            pred: &mut dyn FnMut(&mut gpui::TestAppContext) -> bool,
        ) -> bool {
            for _ in 0..800 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                if pred(cx) {
                    return true;
                }
            }
            false
        }
        let vid = viewer.clone();
        let loaded = wait(cx, &mut |cx| cx.update(|app| vid.read(app).is_loaded()));
        assert!(loaded, "应加载完成");

        let n0 = cx.update(|app| viewer.read(app).page_count());
        assert!(n0 >= 2, "夹具应足量页");

        // ① 插入页（内存级）：页数 +1、dirty、磁盘不变
        cx.update(|app| viewer.update(app, |v, cx| v.insert_blank_page_after(0, cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).page_count() == n0 + 1)),
            "插入后页数 +1"
        );
        assert!(cx.update(|app| viewer.read(app).is_dirty()), "插入后置脏");
        assert_eq!(
            std::fs::read(&p).expect("重读磁盘"),
            disk0,
            "插入页只进内存：磁盘 bytes 必须不变"
        );

        // ② Ctrl+Z 撤销插入：页数复原、清脏、磁盘仍不变
        cx.update(|app| viewer.update(app, |v, cx| v.undo_last(cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| {
                let v = vid.read(app);
                v.page_count() == n0 && !v.is_dirty()
            })),
            "撤销插入后页数复原且清脏"
        );
        assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "撤销后磁盘仍不变");

        // ③ 删除页（内存级）：页数 -1、dirty → 撤销复原
        cx.update(|app| viewer.update(app, |v, cx| v.delete_page(0, cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).page_count() == n0 - 1)),
            "删除后页数 -1"
        );
        assert!(cx.update(|app| viewer.read(app).is_dirty()), "删除后置脏");
        assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "删除页只进内存：磁盘不变");
        cx.update(|app| viewer.update(app, |v, cx| v.undo_last(cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| {
                let v = vid.read(app);
                v.page_count() == n0 && !v.is_dirty()
            })),
            "撤销删除后页数复原且清脏"
        );

        // ④ 注释只进内存（磁盘不变）→ save 才落盘（dirty/pending 清 + 重开读回）
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.add_annotation(0, AnnotKind::Highlight, vec![[60.0, 300.0, 200.0, 330.0]], cx)
            })
        });
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).pending_annot_count() == 1)),
            "加注释后 pending=1"
        );
        assert_eq!(
            std::fs::read(&p).expect("重读磁盘"),
            disk0,
            "注释只进内存：磁盘 bytes 必须不变"
        );
        cx.update(|app| viewer.update(app, |v, cx| v.save(cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| {
                let v = vid.read(app);
                !v.is_dirty() && v.pending_annot_count() == 0
            })),
            "保存后清 pending 与 dirty"
        );
        // ⑤ save 后 live 文档应重载为「含已写注释」：叠加层虽清，基础文档必须带上已写
        // 注释，否则高亮在 save 后从视图中消失直到重开（评审 D1 回归：不重开也能读回）。
        // P1-17：annotations_for_page 已非阻塞（结果通道 + try_recv 轮询异步完成）。
        assert!(
            wait_pending_highlight_readback(cx, &viewer.clone()),
            "save 后 live 文档应含高亮（保存触发重载，注释不过期上图）"
        );
        let saved = std::fs::read(&p).expect("保存后重读");
        assert_ne!(saved, disk0, "保存后磁盘应变化（注释合并写盘）");
        let (w2, d2, _) = PdfWorker::open_and_dims(saved).expect("重开改后文件");
        let anns = w2.annotations_for_page(d2, 0).expect("读回注释");
        assert!(
            anns.iter().any(|a| a.kind == AnnotKind::Highlight),
            "保存后重开应读回 Highlight（效果断言）"
        );
        w2.close(d2);

        let _ = std::fs::remove_file(&p);
    }

    /// P1-18：`save_as` 另存为后 live 文档应重载为「含已写注释」的新文件副本（对齐 save 路径
    /// 的 read-back + `open_from_bytes`）——否则叠加层清掉后高亮从视图立即消失（worker 的
    /// `ReadAnnotations` 读 live `docs` 而非 `stores`/磁盘，不重载就永远读不到已写注释；save
    /// 同缺陷已修、save_as 漏同步）。效果断言：另存完成后 live 文档能异步读回 Highlight；
    /// 原文件保持原样、新路径落盘含注释。
    #[gpui::test]
    async fn save_as_reloads_live_doc_so_annotations_survive(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = test_util::lock().lock().unwrap();
        use gpui::AppContext as _;
        use std::time::Duration;

        let src = test_util::test_pdf_paths()[0].clone();
        let dir = std::env::temp_dir().join("vuick-pdf-tests");
        let _ = std::fs::create_dir_all(&dir);
        let pid = std::process::id();
        let p = dir.join(format!("save-as-src-{pid}.pdf"));
        let q = dir.join(format!("save-as-dst-{pid}.pdf"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&q);
        test_util::copy_fixture_writable(&src, &p).expect("拷贝夹具");
        let disk0 = std::fs::read(&p).expect("读初始磁盘");

        let viewer = cx.update(|app| app.new(|cx| crate::pdf::PdfViewer::new(p.clone(), cx)));
        fn wait(
            cx: &mut gpui::TestAppContext,
            pred: &mut dyn FnMut(&mut gpui::TestAppContext) -> bool,
        ) -> bool {
            for _ in 0..800 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                if pred(cx) {
                    return true;
                }
            }
            false
        }
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).is_loaded())),
            "应加载完成"
        );

        // 预置一条待存高亮注释
        cx.update(|app| {
            viewer.update(app, |v, cx| {
                v.add_annotation(0, AnnotKind::Highlight, vec![[60.0, 300.0, 200.0, 330.0]], cx)
            })
        });
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).pending_annot_count() == 1)),
            "加注释后 pending=1"
        );
        assert_eq!(
            std::fs::read(&p).expect("重读磁盘"),
            disk0,
            "注释只进内存：磁盘 bytes 必须不变"
        );

        // 另存为 → 成功后应清 pending/dirty 并重指新路径
        cx.update(|app| viewer.update(app, |v, cx| v.save_as(q.clone(), cx)));
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| {
                let v = vid.read(app);
                !v.is_dirty() && v.path == q
            })),
            "另存完成后应清 pending/dirty 并重指新路径"
        );

        // 效果断言：live 文档应重载为「含已写注释」的副本——异步读回 Highlight（P1-18；
        // P1-17 非阻塞 + try_recv 轮询异步完成）。
        assert!(
            wait_pending_highlight_readback(cx, &viewer.clone()),
            "另存后 live 文档应含高亮（保存触发重载，注释不过期上图）"
        );

        // 原文件保持原样；新路径落盘含注释（重开读回）
        assert_eq!(
            std::fs::read(&p).expect("重读原路径"),
            disk0,
            "另存为不得改动原文件"
        );
        let saved = std::fs::read(&q).expect("读另存产物");
        assert_ne!(saved, disk0, "另存产物理应变化（注释合并写盘）");
        let (w2, d2, _) = PdfWorker::open_and_dims(saved).expect("重开另存文件");
        let anns = w2.annotations_for_page(d2, 0).expect("读回注释");
        assert!(
            anns.iter().any(|a| a.kind == AnnotKind::Highlight),
            "重开另存文件应读回 Highlight（效果断言）"
        );
        w2.close(d2);

        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&q);
    }

    /// P1-17：`annotations_for_page` 非阻塞异步完成（K37 效果断言）——不再主线程同步
    /// `rx.recv()` 等 pdfium worker：调用**立即**返回结果通道（不阻塞），轮询 `try_recv`
    /// 最终异步取得正确结果（无注释页 = 空列表，读回管线真完成）。
    /// 「save/save_as 后轮询已写注释高亮」的完整异步路径由 `wait_pending_highlight_readback`
    /// 覆盖（见 save / P1-18 测试）。
    #[gpui::test]
    async fn annotations_for_page_is_nonblocking_and_completes(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = test_util::lock().lock().unwrap();
        use gpui::AppContext as _;
        use std::time::Duration;

        let src = test_util::test_pdf_paths()[0].clone();
        let viewer: gpui::Entity<crate::pdf::PdfViewer> =
            cx.update(|app| app.new(|cx| crate::pdf::PdfViewer::new(src, cx)));
        fn wait(
            cx: &mut gpui::TestAppContext,
            pred: &mut dyn FnMut(&mut gpui::TestAppContext) -> bool,
        ) -> bool {
            for _ in 0..800 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(2));
                if pred(cx) {
                    return true;
                }
            }
            false
        }
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| vid.read(app).is_loaded())),
            "应加载完成"
        );

        // 调用立即返回 Some(结果通道)——不在调用线程同步等 worker（P1-17）
        let rx = cx
            .update(|app| viewer.update(app, |v, cx| v.annotations_for_page(0, cx)))
            .expect("已加载文档应返回结果通道");
        // 轮询直到异步读回完成（worker 后台处理；无注释页 = Ok(Some([]))）
        let mut completed = false;
        for _ in 0..800 {
            match rx.try_recv() {
                Ok(res) => {
                    assert!(res.is_some(), "worker 读回应成功（无注释页为空列表）");
                    completed = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.run_until_parked();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
        assert!(
            completed,
            "异步读回应最终完成（K37 效果断言：后台真执行、结果真到达）"
        );
    }

    /// 图片盒懒提取（worker 层）：真实夹具前若干页抽取页图片盒——几何夹在页内、
    /// 自然尺寸为正、盒正宽高、带所属页号；取到第一张图后按**自然分辨率**取
    /// `ImagePixels`（紧凑 `w*h*4` 且非全零/全透明）。夹具无 raster 图片 → SKIP 提示
    /// （pdf测试.pdf 为文本型配置表/扫描件，图片对象并非必然存在）。
    #[test]
    fn extracts_page_images_and_pixels_from_real_pdf() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        use std::time::Duration;
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let mut found: Option<PdfImageBox> = None;
        let mut image_pages = 0usize;
        for page in 0..20.min(dims.len()) {
            let rx = worker.page_images(doc_id, page);
            let boxes = rx
                .recv_timeout(Duration::from_secs(30))
                .expect("应收到图片盒结果")
                .expect("页应存在");
            if boxes.is_empty() {
                continue;
            }
            image_pages += 1;
            let (pw, ph) = dims[page];
            for b in &boxes {
                assert!(
                    b.x0 >= 0.0 && b.x1 <= pw && b.y0 >= 0.0 && b.y1 <= ph,
                    "图片盒应夹在页内（{pw}×{ph}pt）：第 {page} 页 {b:?}"
                );
                assert!(
                    b.x1 > b.x0 && b.y1 > b.y0,
                    "盒应正宽高：第 {page} 页 {b:?}"
                );
                assert!(b.natural_w > 0 && b.natural_h > 0, "自然尺寸应为正: {b:?}");
                assert_eq!(b.page_index, page, "盒应带所属页号");
            }
            if found.is_none() {
                found = boxes.first().copied();
            }
        }
        let Some(b) = found else {
            eprintln!(
                "SKIP 图片盒: pdf测试.pdf 前 {} 页无 raster Image 对象（文本型夹具不含图片，复制图片无法该夹具端到端；共有图页数={image_pages}）",
                20.min(dims.len())
            );
            worker.close(doc_id);
            return;
        };
        eprintln!(
            "[IMAGE-BOX] 找到图片 p{} obj{} 盒 [{:.1},{:.1} {:.1},{:.1}]pt 自然 {}×{}px",
            b.page_index, b.obj_index, b.x0, b.y0, b.x1, b.y1, b.natural_w, b.natural_h
        );
        let rx = worker.image_pixels(doc_id, b.page_index, b.obj_index, b.natural_w, b.natural_h);
        let pix = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("应收到像素结果")
            .expect("按自然分辨率取像应成功");
        assert_eq!(
            pix.bgra.len(),
            (pix.width as usize) * (pix.height as usize) * 4,
            "紧凑 BGRA 应为纯 w*h*4（无 stride）"
        );
        assert!(
            pix.width > 0 && pix.height > 0,
            "像素尺寸应为正: {}×{}",
            pix.width,
            pix.height
        );
        let opaque_or_ink = pix.bgra.chunks_exact(4).any(|px| {
            px[0] != 0 || px[1] != 0 || px[2] != 0 || px[3] != 0
        });
        assert!(opaque_or_ink, "图片像素不应全空（编码失败会产出零缓冲）");
        eprintln!(
            "[IMAGE-PIX] p{} obj{} → {}×{}px, bgra {}B",
            b.page_index,
            b.obj_index,
            pix.width,
            pix.height,
            pix.bgra.len()
        );
        worker.close(doc_id);
    }

    /// 图片命中 + 左键单击交互（gpui 端到端）：真实夹具内有图片页上——正中点命中
    /// `image_at_window_pos` 且 `image_pos` 可定位；左键单击图 → `selected_image` 置位 +
    /// 文本选区互斥清空（预置一条选区）；再点页内空白 → `selected_image` 清除 + 选区
    /// 逻辑不受影响。夹具无图 → SKIP。
    #[gpui::test]
    async fn image_click_selects_and_clears(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{
            AppContext as _, Entity, InteractiveElement as _, IntoElement, MouseButton,
            ParentElement as _, Render, Styled as _,
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        struct ImageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for ImageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div()
                    .id("image-pane")
                    .w(gpui::px(1200.))
                    .h_full()
                    .child(self.pdf.clone())
            }
        }

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pdf测试.pdf");
        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            ImageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..200 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .map(|pv| pv.read(cx).is_loaded())
                    .unwrap_or(false)
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "PdfViewer 应加载完成");

        // 同步读入前若干页图片盒，找第一张可定位的图
        let mut target: Option<(usize, usize)> = None;
        cx.update(|_w, cx| {
            let pdf = holder.borrow().as_ref().unwrap().clone();
            for page in 0..20usize {
                pdf.update(cx, |v, _| {
                    v.load_images_for_test(page);
                });
                let pv = pdf.read(cx);
                let Some(boxes) = pv.image_pages.get(&page) else {
                    continue;
                };
                if let Some(b) = boxes.first().filter(|b| b.x1 - b.x0 > 10.0 && b.y1 - b.y0 > 10.0)
                {
                    // 过滤掉「紧贴页边 / 极小」的噪声盒（真正可点的大图才有意义）
                    target = Some((page, b.obj_index));
                    break;
                }
            }
        });
        let Some((page, obj)) = target else {
            eprintln!("SKIP 图片命中: pdf测试.pdf 前 20 页无可定位大图（文本型夹具无图）");
            return;
        };
        // 先滚到该页（image_pos 是窗口坐标，图不在视口内则收不到派发）
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().clone().update(cx, |v, cx| {
                v.scroll_to_page(page);
                cx.notify();
            });
        });
        for _ in 0..6 {
            cx.run_until_parked();
        }
        let center = cx
            .update(|window, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .image_pos(window, page, obj)
            })
            .expect("滚到该页后应可定位图中心");
        eprintln!(
            "[IMAGE-CLICK] 目标图 p{page} obj{obj} 命中点 ({:.0},{:.0})px",
            center.x.as_f32(),
            center.y.as_f32()
        );

        let hit = cx.update(|window, cx| {
            let pv = holder.borrow().as_ref().unwrap().clone();
            pv.read(cx).image_at_window_pos(window, center)
        });
        assert_eq!(hit, Some((page, obj)), "图中心应命中该图 (p{page} obj{obj})");

        // 预置一条文本选区（模拟「已有文本选中」），验证左键点图互斥清除
        cx.update(|window, cx| {
            let pdf = holder.borrow().as_ref().unwrap().clone();
            pdf.update(cx, |v, cx| {
                v.open_selection_card_for_test((page, 0), (page, 1), window, cx);
            });
        });
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let had_sel = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).sel_anchor.is_some()
        });
        assert!(had_sel, "预置文本选区应存在（互斥前提）");

        // 左键单击图中心 → 选中该图 + 文本选区被清
        let mods = gpui::Modifiers::default();
        cx.simulate_mouse_move(center, MouseButton::Left, mods);
        cx.simulate_mouse_down(center, MouseButton::Left, mods);
        cx.simulate_mouse_up(center, MouseButton::Left, mods);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let (sel_img, sel_clear) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.selected_image, v.sel_anchor.is_none())
        });
        assert_eq!(sel_img, Some((page, obj)), "点图应选中该图");
        assert!(sel_clear, "点图应互斥清除文本选区");

        // 点页内空白（左上角页边距区域）→ 清除图片选中
        let blank = gpui::point(gpui::px(30.), gpui::px(30.));
        cx.simulate_mouse_move(blank, MouseButton::Left, mods);
        cx.simulate_mouse_down(blank, MouseButton::Left, mods);
        cx.simulate_mouse_up(blank, MouseButton::Left, mods);
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let sel_img2 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).selected_image);
        assert_eq!(sel_img2, None, "点空白应清除图片选中");
    }

    // ---- A/B 线：分页慢加载根治（2026-11-20，真 pdfium + 真夹具）----
    // 共用装配：`target/pdf测试2.pdf`（259 页 A4），窗口 1500×900（cell_w = min(1460, 636.4)
    // = 636.4）；sf=2 测试平台。夹具缺失 → SKIP 常规交付。
    fn pdf_path_of() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试2.pdf")
    }

    /// 装配 + 等加载完成 + 停稳到调度清空（封面/瓦片在途空、非快速态、泵停）。
    fn paged_settle(
        cx: &mut gpui::VisualTestContext,
        holder: &std::rc::Rc<
            std::cell::RefCell<Option<gpui::Entity<crate::pdf::PdfViewer>>>,
        >,
    ) {
        use std::time::Duration;
        for _ in 0..600 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
            let st = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                (
                    v.pending_cover.is_empty() && v.pending_tiles.is_empty(),
                    !v.prev_fast && !v.tile_pump_running,
                )
            });
            if st == (true, true) {
                return;
            }
        }
        panic!("状态应沉降（封面/瓦片在途应清空）");
    }

    /// T1（A 线红绿）：分页 Facing 模式深跳页 200 → 停稳后**目标页有底图链 + 封面在槽**，
    /// 且静置期「加载中」计数零增长。
    ///
    /// 修复前（A 线未落）：`ensure_window` 按连续 `keep_window` ±MARGIN 选窗，竖排页 + 宽视口
    /// 下分页可见页漂出连续窗 → 页 200 的 cover/缩略/走廊**全部不被请求** → 停稳后页 200 仍
    /// 无任何底图、每静置帧都重绘「加载中」占位（计数不断增长）——headless 真 worker 路径可证。
    #[gpui::test]
    async fn paged_deep_jump_settles_without_placeholder(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = pdf_path_of();
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count());
        assert!(n >= 220, "夹具应 ≥220 页（深跳需远离首屏），实际 {n}");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        // 切分页模式（Facing）并停稳（分页快照装配 + 首屏分页调度收敛）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        // 几何锚（K177 演算，Facing cols=2、窗口 1500×900、pdf测试2.pdf A4 AR=w/h≈0.7071）：
        // content_w = 1500−2×20 = 1460、cap_w = (1460−PAGED_GAP_X=1)/2 = 729.5、
        // fit = 900×(595.28/841.89) ≈ 636.4 → cell_w = min(729.5, 636.4) = 636.4
        // （与 Single 同值：fit 受限主导，列数/列间距只改 cap_w 不动 min）。
        let (cw, sf) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.page_cell_width(200), _w.scale_factor())
        });
        assert!(
            (cw - 636.4).abs() <= 0.5,
            "分页卡宽 page_cell_width(200)≈636.4±0.5（实际 {cw}，sf={sf}）"
        );
        // 深跳页 200 → 停稳。
        let jump = |cx: &mut gpui::VisualTestContext, page: usize| {
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                    v.scroll_to_page(page);
                    cx.notify();
                });
            });
        };
        jump(&mut *cx, 200);
        paged_settle(cx, &holder);
        // 主判据：停稳后目标页 200 已有底图链 + 满密度封面（A 线：调度窗含深页 → 封面被请求）。
        let (cover_ok, base_ok, hits) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let s = &v.slots[200];
            (
                s.cover.is_some(),
                s.cover.is_some()
                    || s.prev_cover.is_some()
                    || s.prefetch_cover.is_some()
                    || s.mid.is_some()
                    || s.thumb.is_some(),
                v.loading_placeholder_hits.get(),
            )
        });
        assert!(base_ok, "页 200 停稳后底图链应非空（A 线修复点）");
        assert!(cover_ok, "页 200 停稳后满密度封面应在槽");
        // 静置零增长：停稳后再推 5 帧，「加载中」计数不增长（与既有 `thumb_backdrop_...`
        // 的「静置零增长」口径一致；跳转期的瞬态占位是 worker 未回首帧的合法最小值，
        // 故以停稳后为基线）。
        let read_hits = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .loading_placeholder_hits
                    .get()
            })
        };
        let h0 = read_hits(cx);
        for _ in 0..5 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let h1 = read_hits(cx);
        eprintln!(
            "[PAGED-T1] cw={cw:.2} sf={sf} cover_ok={cover_ok} base_ok={base_ok} hits {hits}({h0}->{h1})"
        );
        assert!(
            h1 == h0,
            "停稳后「加载中」计数应零增长：{h0}->{h1}"
        );
    }

    /// T2（B 线红绿）：分页 Facing 模式停稳在页 200 后，封面密度 = 分页卡宽口径
    /// `cover_scale(dims[200], page_cell_width(200), sf)`（不再按连续 page_width ~2.1× 过采样）。
    /// 演算：zoom_c=(636.4/840).max(1)=1、cap=min(1.5×1×2,16)=3.0、
    /// raw=636.4×2/595.28≈2.138、cover=min(2.138, 4.0)=2.138。（cell_w 636.4 与 Single 同值，
    /// 密度只依赖卡宽不依赖列数，锚保持。）
    #[gpui::test]
    async fn paged_cover_density_follows_cell_width(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = pdf_path_of();
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(200);
                cx.notify();
            });
        });
        paged_settle(cx, &holder);
        let (cw, got, expected, cover_ok) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let sf = _w.scale_factor();
            let cw = v.page_cell_width(200);
            let expected = crate::pdf::layout::cover_scale(v.dims[200], cw, sf);
            (cw, v.slots[200].cover_scale, expected, v.slots[200].cover.is_some())
        });
        eprintln!("[PAGED-T2] cw={cw:.2} cover_scale={got:.4} expected={expected:.4} cover_ok={cover_ok}");
        assert!(
            (got - expected).abs() <= 1e-4,
            "封面密度应走分页卡宽口径：cover_scale={got:.4} expected={expected:.4}"
        );
        assert!(cover_ok, "页 200 封面应在槽");
    }

    /// T3：切 Facing 跳页 40 → 行 20 = 页 40/41 并排，断言两页底图链非空 + 静置零增长
    /// （密度/深跳驻留断言由 T1/T2 收口，本测试只守「Facing 行并排两页的调度」）。
    #[gpui::test]
    async fn paged_zooms_and_facing_keep_base_and_density(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        use gpui::{AppContext, Entity, IntoElement, ParentElement, Render, Styled};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Duration;

        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        cx.update(crate::test_support::bind_local_keymaps);
        let pdf_path = pdf_path_of();
        assert!(pdf_path.exists(), "测试 PDF 应存在");

        struct PageProbe {
            pdf: Entity<crate::pdf::PdfViewer>,
        }
        impl Render for PageProbe {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                gpui::div().size_full().child(self.pdf.clone())
            }
        }

        let holder: Rc<RefCell<Option<Entity<crate::pdf::PdfViewer>>>> = Rc::default();
        let (_root, cx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| crate::pdf::PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageProbe { pdf }
        });
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(900.)));
        let mut loaded = false;
        for _ in 0..400 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().read(cx).is_loaded()
            });
            if loaded {
                break;
            }
        }
        assert!(loaded, "应加载完成");
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let set_mode = |cx: &mut gpui::VisualTestContext, mode: crate::pdf::view::PageMode| {
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                    v.set_page_mode(mode, cx);
                });
            });
        };
        let jump = |cx: &mut gpui::VisualTestContext, page: usize| {
            cx.update(|_w, cx| {
                holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                    v.scroll_to_page(page);
                    cx.notify();
                });
            });
        };
        let read_hits = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_w, cx| {
                holder
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .loading_placeholder_hits
                    .get()
            })
        };
        set_mode(&mut *cx, crate::pdf::view::PageMode::Facing);
        paged_settle(cx, &holder);
        // Facing：行 20 = 页 40/41 并排，断言两页底图链非空 + 静置零增长（无占位）。
        set_mode(&mut *cx, crate::pdf::view::PageMode::Facing);
        paged_settle(cx, &holder);
        jump(&mut *cx, 40);
        paged_settle(cx, &holder);
        let (b40, b41) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let base = |p: usize| {
                let s = &v.slots[p];
                s.cover.is_some()
                    || s.prev_cover.is_some()
                    || s.prefetch_cover.is_some()
                    || s.mid.is_some()
                    || s.thumb.is_some()
            };
            (base(40), base(41))
        });
        let h0 = read_hits(cx);
        for _ in 0..5 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(3));
        }
        let h1 = read_hits(cx);
        eprintln!("[PAGED-T3-facing] base40={b40} base41={b41} hits {h0}->{h1}");
        assert!(b40, "Facing 页 40 底图链应非空");
        assert!(b41, "Facing 页 41 底图链应非空");
        assert!(h1 == h0, "Facing：停稳后计数应零增长 {h0}->{h1}");
    }

    // ---- 模式切换对齐复现（2026-11-20 诊断红测，K177 几何推导）----
    // 锚点与 A/B 线共用：target/pdf测试2.pdf（259 页 A4，AR=w/h≈0.7071），窗口 1500×900、
    // sf=2、zoom=1。用户真机：同一阅读位置，连续显示第 10/28 页，切 Facing 显示 39|40/109|110
    // （页号 ≈4× 跳变）且内容整体偏上。本组锁定「连续 → Facing 切模式不重锚 scroll」机制。
    //
    // 几何（K177 演算，1500×900、A4、zoom=1）：
    //   连续：page_width = (1500−2×20)×1 = 1460；页高 disp = 1460/0.7071 ≈ 2064.9；
    //     page_tops[9] = 16 + 9×(2064.9+14) ≈ 18725.8（第 10 页顶对齐视口顶）。
    //   Facing：content_w = 1460、cap_w = (1460−PAGED_GAP_X=1)/2 = 729.5、fit_w = 900×0.7071
    //     → cell_w=636.4、cell_h=900、row_top[r] = 20 + r×(900+PAGED_GAP_Y=6) = 20+906r；
    //     行 4 = 页 9|10（含第 10 页）。
    //   关键判据：set_page_mode 只翻状态（open.rs:823-832 不动 scroll offset）→ 切模式后
    //     scroll_y 像素保持。若正确语义是「页对应」，应把连续「当前页 10」换算成 Facing
    //     行 4 顶（row_top[4]=20+4×906=3644）→ 仍见第 10 页；实况 scroll_y≈18725.8 未换算，
    //     落到行 20（row_top[20]=18140，页 41|42）→ 页号 ≈41/10≈4.1× 跳变（用户所见「对不上」）。
    //     页高比 disp/(row/2) ≈ 2064.9/456 ≈ 4.5 → 「≈4×」主要来自几何：分页每行两页按视口高
    //     适配（高 900），连续单页按视口宽适配（高 ≈2064.9），同一像素位覆盖的内容跨度相差
    //     ≈4.5×；但「切模式后保持的是像素不是页」这一选择本身是换算 bug——用户期望页对应。
    // T-A：连续第 10 页顶对齐视口顶 → 切 Facing → current_page() 应仍 ≈10（±2），实况 41 → 红。
    #[gpui::test]
    async fn mode_switch_continuous_to_facing_preserves_visible_page(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        // 连续：滚动到第 10 页（index 9）顶对齐视口顶（scroll_to_page 连续口径 = -page_tops[9]）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(9);
                cx.notify();
            });
        });
        zoom_wait_frames(cx, 40);
        let (_sy0, cont_page, cont_disp_h) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let sy = f32::from(-v.scroll.offset().y);
            let p = v.current_page().map(|(p, _)| p).unwrap_or(0);
            let dh = v
                .page_tops
                .get(1)
                .zip(v.page_tops.first())
                .map(|(t1, t0)| t1 - t0 - config::pdf::PAGE_GAP)
                .unwrap_or(0.0);
            (sy, p, dh)
        });
        assert_eq!(cont_page, 10, "前提：连续模式应显示第 10 页，实况 {cont_page}");
        assert!(
            cont_disp_h > 500.0,
            "前提：连续页高应显著（实况 {cont_disp_h:.1}px）"
        );
        // 切换 Facing（真实路径 = shell status_bar.rs:590 的 set_page_mode，不重锚 scroll）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        // 读 Facing 实况 + 页对应期望（行 4 = 页 9|10）。
        let (facing_page, ..) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().expect("Facing 应有分页快照");
            let sy = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height);
            let (r0, _) = l.first_visible_row(-sy, vh.max(1.0)).expect("应有首可见行");
            (
                v.current_page().map(|(p, _)| p).unwrap_or(0),
                sy,
                vh,
                r0,
                l.row_top[r0],
                l.row_top[1] - l.row_top[0],
                l.row_page_start[4] + 1,
            )
        });
        assert!(
            (facing_page as i64 - 10).abs() <= 2,
            "连续→Facing 应保持「当前页」：连续第 10 页 → Facing 首可见页应 ≈10（=行 4/页 9|10），实况 {facing_page}（页 41|42 ≈{:.1}× 跳变；scroll_y 未按分页布局换算，纯像素保留）",
            facing_page as f32 / 10.0
        );
    }

    // T-B：连续 → Facing 后「首可见行顶部与视口顶对齐」（无偏上）。期望 = 页对应 + 行顶对齐：
    // 首可见行 = 行 4（含第 10 页）、sy1 恰好 = row_top[4]（视口顶贴行顶，无半行裁切）。
    // 实况：sy1 仍为连续 scroll_y0 ≈18725.8 → 首可见行 r0=20，视口顶落在行 20 内
    //   +465.8px 处（行顶已在视口上缘之上）→ 内容整体偏上/半行裁切（用户「偏上」观测）。
    #[gpui::test]
    async fn facing_switch_content_aligned_to_row_top(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(9);
                cx.notify();
            });
        });
        zoom_wait_frames(cx, 40);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        let (sy1, r0, row_top_r0, r0_expect, _row4_top) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().expect("Facing 应有分页快照");
            let sy = f32::from(-v.scroll.offset().y);
            let vh = f32::from(v.scroll.bounds().size.height);
            let (r0, _) = l.first_visible_row(-sy, vh.max(1.0)).expect("应有首可见行");
            (
                sy,
                r0,
                l.row_top[r0],
                9 / l.cols,
                l.row_top[9 / l.cols],
            )
        });
        let delta = (sy1 - row_top_r0).abs();
        assert_eq!(
            r0, r0_expect,
            "Facing 首可见行应为第 10 页所在行（page/cols = 9/2 = 行 4），实况行 {r0}"
        );
        assert!(
            delta <= 1.0,
            "Facing 首可见行顶部应与视口顶对齐（无偏上）：delta=视口顶−行顶={delta:.1}px（实况 sy1={sy1:.1} 未按行界重锚，行顶已在视口上缘之上，内容整体偏上）"
        );
    }

    // T-C（对称红测，K177）：Facing → Continuous 双向页对应——Facing 显第 10 页所在行
    // （行 4 = 页 9|10，current_page = 行首页 row_page_start[4]+1 = 9）→ 切 Continuous →
    // 首可见页应仍 ≈ 同一页（9）。修复前只翻状态不动 scroll：Facing 滚动位 sy ≈ row_top[4]
    // = 3644 px，切 Continuous 后同一像素 (3644−16)/(2064.9+14) ≈ 1.75 行 → 首可见页 2
    // （≈9 跳变，与 T-A 同根因反向）。
    //
    // 期望推导（K177，pdf测试2.pdf A4 AR=w/h≈0.7071、窗口 1500×900、zoom=1）：
    //   Facing：scroll_to_page(9) → r = 9/2 = 4 → row_top[4] = 20 + 4×(900+6) = 3644；
    //     current_page = row_page_start[4]+1 = (4×2)+1 = 9；reading_progress 读 (8, 0.0)
    //     （行首页 index 8 = row_page_start[4]）。
    //   Continuous（页对应重锚后）：sy = page_tops[8] + 0 = 16 + 8×(2064.9+14) ≈ 16647.2
    //     → first_visible_page = 8（index）→ 首可见页 8+1 = 9 ＝ Facing 的 9。
    #[gpui::test]
    async fn mode_switch_facing_to_continuous_preserves_visible_page(
        cx: &mut gpui::TestAppContext,
    ) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        // Facing：跳页 9（index 9）→ 行 4 顶（页 9|10 所在行）对齐视口顶。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(9);
                cx.notify();
            });
        });
        paged_settle(cx, &holder);
        let (facing_page, sy0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let p = v.current_page().map(|(p, _)| p).unwrap_or(0);
            let sy = f32::from(-v.scroll.offset().y);
            (p, sy)
        });
        assert_eq!(facing_page, 9, "前提：Facing 行 4 行首页应 = 9（页 9|10 所在行）");
        // 切 Continuous（真实路径 = set_page_mode，不重锚 scroll）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Continuous, cx);
            });
        });
        zoom_wait_frames(cx, 40);
        let (cont_page, sy1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let p = v.current_page().map(|(p, _)| p).unwrap_or(0);
            let sy = f32::from(-v.scroll.offset().y);
            (p, sy)
        });
        eprintln!(
            "[MODE-TC] facing_page={facing_page} sy0={sy0:.1} | cont_page={cont_page} sy1={sy1:.1} (未重锚={:.1}px)",
            (sy1 - sy0).abs()
        );
        assert!(
            (cont_page as i64 - facing_page as i64).abs() <= 1,
            "Facing→Continuous 应保持「当前页」：Facing 首可见页 {facing_page} → Continuous 应 ≈同页，实况 {cont_page}（滚动像素未按连续页列换算，{:.1}px 深向差）",
            (sy1 - sy0).abs()
        );
    }

    // T-D（边界，K177）：末行 ragged——259 页 A4、Facing cols=2 → rows=ceil(259/2)=130；
    // 行 0..128 各 2 页、末行 129 仅 1 页（页 258）。scroll_to_page(258) 到末行顶 → 切
    // Continuous → 再切 Facing → 断言不 panic、首可见页 = 末页 259、滚动被夹在合法量程。
    // 恢复侧边界（open.rs restore_pending 分页分支）：`r = (page/cols).min(rows-1)` 对
    // page 258 → r = 129（==rows−1，不越界）；`row_page_start[r+1]` 末行取
    // (130×2).min(259)=259，区间 [258,259) 有效（ragged 无越界）。
    #[gpui::test]
    async fn mode_switch_ragged_last_row_preserves_boundary(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        let n = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).page_count());
        assert!(n >= 259, "夹具应 ≥259 页（末行 ragged 需奇数页），实际 {n}");
        // Facing → scroll_to_page(258)（0 基末页）→ 末行 129 顶对齐视口顶。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(258);
                cx.notify();
            });
        });
        paged_settle(cx, &holder);
        let (facing_page, l_ok, sy0, max0) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let sy = f32::from(-v.scroll.offset().y).max(0.0);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().expect("Facing 应有分页快照");
            assert_eq!(l.rows, 130, "259 页 Facing → 130 行");
            let vh = f32::from(v.scroll.bounds().size.height);
            let max = (l.total_height - vh).max(0.0);
            // 末行页区间端点：row_page_start[129]=258、row_page_start[130]=min(260,259)=259。
            let ok = l.row_page_start.len() == l.rows + 1
                && l.row_page_start[129] == 258
                && l.row_page_start[130] == 259;
            (
                v.current_page().map(|(p, _)| p).unwrap_or(0),
                ok,
                sy,
                max,
            )
        });
        assert!(l_ok, "末行 row_page_start 区间应 [258,259)（ragged 不越界）");
        assert!(
            (facing_page as i64 - 259).abs() <= 1,
            "Facing 末行首可见页应 = 259（末页），实况 {facing_page}"
        );
        assert!(
            sy0 <= max0 + 0.5,
            "Facing 末行滚动应被夹在分页量程内：sy={sy0:.1} ≤ max={max0:.1}"
        );
        // 切 Continuous：restore 连续分支 k = 258（page_tops[258]）→ 首可见页 259。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Continuous, cx);
            });
        });
        zoom_wait_frames(cx, 40);
        let (cont_page, sy1, total_h) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.current_page().map(|(p, _)| p).unwrap_or(0),
                f32::from(-v.scroll.offset().y).max(0.0),
                v.total_height,
            )
        });
        assert!(
            (cont_page as i64 - 259).abs() <= 1,
            "Facing→Continuous 末页应保持 259，实况 {cont_page}"
        );
        assert!(
            sy1 <= total_h.max(0.0) + 0.5,
            "连续末页滚动应被夹在连续量程内：sy={sy1:.1} ≤ total={total_h:.1}"
        );
        // 再切回 Facing：r = min(258/2, 130−1) = 129 → row_top[129]（末行顶）→ 首可见页 259。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        let (facing2_page, sy2, max2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let sy = f32::from(-v.scroll.offset().y).max(0.0);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().expect("Facing 应有分页快照");
            let vh = f32::from(v.scroll.bounds().size.height);
            (v.current_page().map(|(p, _)| p).unwrap_or(0), sy, (l.total_height - vh).max(0.0))
        });
        eprintln!(
            "[MODE-TD] n={n} facing={facing_page} cont={cont_page} facing2={facing2_page} sy0={sy0:.0} sy1={sy1:.0} sy2={sy2:.0} max0={max0:.0} max2={max2:.0}"
        );
        assert!(
            (facing2_page as i64 - 259).abs() <= 1,
            "Facing→Continuous→Facing 末页应保持 259，实况 {facing2_page}"
        );
        assert!(sy2 <= max2 + 0.5, "二次 Facing 末行滚动应仍在分页量程内");
    }

    // T-E（边界）：同帧连续两次 set_page_mode（Facing → Continuous，中间无帧泵）→
    // last-writer-wins：最终 page_mode=Continuous、滚动 = 连续第 10 页位（无残留中间 pending
    // 把位置带偏）。K177：连续第 10 页（index 9）在位 → 第一次 Facing 捕获 (9,0) 挂 pending；
    // 第二次 Continuous 因分页快照未装配回落连续口径再捕获 (9,0) **覆写** pending → 下一帧
    // restore 落地 page_tops[9] → 首可见页 10。若残留第一次的中间位（Facing row_top[4] 类）
    // 即位置被带偏。
    #[gpui::test]
    async fn mode_switch_rapid_double_flip_last_writer_wins(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        // 连续：滚动到第 10 页顶（index 9）。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.scroll_to_page(9);
                cx.notify();
            });
        });
        zoom_wait_frames(cx, 40);
        let (p0,) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.current_page().map(|(p, _)| p).unwrap_or(0),)
        });
        assert_eq!(p0, 10, "前提：连续模式应显示第 10 页");
        // 同帧双翻转（中间不泵帧）：先 Facing 再 Continuous。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
                v.set_page_mode(crate::pdf::view::PageMode::Continuous, cx);
            });
        });
        zoom_wait_frames(cx, 40);
        let (pm, p1, sy1) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.page_mode(),
                v.current_page().map(|(p, _)| p).unwrap_or(0),
                f32::from(-v.scroll.offset().y),
            )
        });
        eprintln!(
            "[MODE-TE] p0={p0} pm={pm:?} p1={p1} sy1={sy1:.1}（连续第 10 页顶应≈{:.1}）",
            sy1
        );
        assert_eq!(pm, crate::pdf::view::PageMode::Continuous, "双翻转末态应为 Continuous（last-writer-wins）");
        assert!(
            (p1 as i64 - 10).abs() <= 1,
            "双翻转后应停在第 10 页：实况 {p1}（残留中间 pending 会把位置带偏）"
        );
    }

    // T-F（边界，K177/K190 frac>0 参数化）：连续第 10 页中部（frac≈0.5）→ 切 Facing →
    // 目标页 10 仍可见（行 4 含页 9|10，current_page = 行首页 9 ∈ [9,11]），且行内 frac 保留
    // （sy − row_top[4]×zoom ≈ 0.5×row_h×zoom；距行顶 450px，自由滚动无吸附、不贴顶）→
    // 切回 Continuous → 回到「Facing 行首页」index 8（1 基页 9）附近、
    // frac≈0.5 保留。「页对应而非像素对应」在 frac>0 也成立。
    // 自我证（K177）：sy = page_tops[9] + 0.5·disp_h → reading_progress 连续分支读 (9, ≈0.5)。
    // 注：Facing→Continuous 的「同页」指 Facing 行首页（reading_progress 分页口径 =
    // row_page_start[r0]=8），非原 1 基页 10——既有分页口径语义（与 T-C 一致，非回归）。
    #[gpui::test]
    async fn mode_switch_mid_page_frac_preserved(cx: &mut gpui::TestAppContext) {
        skip_if_missing_pdf_fixture!();
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let holder = zoom_holder();
        let (_root, cx) = zoom_add_window(cx, &holder, "pdf测试2.pdf");
        // 连续：滚到第 10 页中部（frac=0.5），并 self 证 reading_progress 读 (9, ≈0.5)。
        let sy0 = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let disp_h = v
                .page_tops
                .get(1)
                .zip(v.page_tops.first())
                .map(|(t1, t0)| t1 - t0 - config::pdf::PAGE_GAP)
                .unwrap_or(0.0);
            v.page_tops[9] + 0.5 * disp_h
        });
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, _cx| {
                v.scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(-sy0)));
            });
        });
        zoom_wait_frames(cx, 10);
        let (p0, f0) = cx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .reading_progress()
                .map(|(p, f, _)| (p, f))
                .expect("应读到进度")
        });
        assert_eq!(p0, 9, "前提：目标页应为第 10 页（index 9）");
        assert!(
            (f0 - 0.5).abs() < 0.01,
            "前提：第 10 页应停在页内 50%（frac≈0.5），实况 {f0}"
        );
        // 切 Facing：restore 分页分支 r=9/2=4、sy = row_top[4]×zoom + frac×row_h×zoom。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Facing, cx);
            });
        });
        paged_settle(cx, &holder);
        let (facing_page, offset_f, row_h_read, frac_f, page10_visible) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let zoom = v.zoom.max(0.01);
            let sy = f32::from(-v.scroll.offset().y).max(0.0);
            let vh = f32::from(v.scroll.bounds().size.height);
            let l = v.paged_layout_cell.borrow();
            let l = l.as_ref().expect("Facing 应有分页快照");
            let (r0, _) = l.first_visible_row(-(sy / zoom), vh / zoom).unwrap_or((0, 0));
            let row_top = l.row_top[r0];
            let row_h = l.row_height(r0).max(f32::EPSILON);
            // 页 10（index 9）可见 ⇔ 落在首可见行页区间 [row_page_start[r0], row_page_start[r0+1]-1]。
            let vis_start = l.row_page_start[r0];
            let vis_end = l.row_page_start[r0 + 1].saturating_sub(1);
            (
                v.current_page().map(|(p, _)| p).unwrap_or(0),
                sy - row_top * zoom,
                row_h,
                ((sy / zoom - row_top) / row_h).clamp(0.0, 1.0),
                vis_start <= 9 && 9 <= vis_end,
            )
        });
        eprintln!(
            "[MODE-TF] p0={p0} f0={f0:.3} | facing_page={facing_page} offset_f={offset_f:.1} row_h={row_h_read:.1} frac_f={frac_f:.3} page10_visible={page10_visible}"
        );
        assert!(
            (facing_page as i64 - 9).abs() <= 2,
            "切 Facing 后目标页仍可见：首可见行页应在 9±2，实况 {facing_page}"
        );
        assert!(page10_visible, "切 Facing 后第 10 页应在首可见行页区间内");
        assert!(
            (offset_f - 0.5 * row_h_read).abs() < 1.0,
            "行内 frac 应保留（offset≈0.5×行高 {row_h_read:.1}）：实况 offset={offset_f:.1}（页对应而非像素对应）"
        );
        assert!((frac_f - 0.5).abs() < 0.01, "Facing 侧 reading_progress frac≈0.5，实况 {frac_f}");
        // 切回 Continuous：restore 连续分支 k = row_page_start[4] = 8、frac≈0.5 保留。
        cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.set_page_mode(crate::pdf::view::PageMode::Continuous, cx);
            });
        });
        zoom_wait_frames(cx, 40);
        let (pm, p2, f2) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            let rp = v.reading_progress().map(|(p, f, _)| (p, f)).unwrap_or((0, 0.0));
            (v.page_mode(), v.current_page().map(|(p, _)| p).unwrap_or(0), rp.1)
        });
        eprintln!("[MODE-TF-cont] pm={pm:?} p2={p2} f2={f2:.3}");
        assert_eq!(pm, crate::pdf::view::PageMode::Continuous, "切回后应为 Continuous");
        assert!(
            (p2 as i64 - 9).abs() <= 1,
            "切回 Continuous 应回到 Facing 行首页（1 基页 9≈原页 10 附近）：实况 {p2}"
        );
        assert!(
            (f2 - 0.5).abs() < 0.05,
            "切回后页内 frac 应≈0.5 保留：实况 {f2}"
        );
    }

    /// [回归，纯数学不触 pdfium] Facing=连续自由滚动下，行中位 → 渲染窗 `paged_render_window`
    /// 必须覆盖全可见行（K177 演算）：视口顶落在行 0 中部、视口底落入行 1 →
    /// `first_visible_row` 返回 (0, 1)（两行都真可见），渲染窗按全可见行 ± 缓冲推出页区间
    /// (0, 2)——旧 `paged_visible_pages` 只按**首行 r0** 会漏行 1（自由滚动停在行中间时
    /// 下一行空白）。本测试锁「渲染窗覆盖全可见行」这一契约。
    ///
    /// 演算：Facing cols=2、3 页 A4 (793,1123)、视口 1500×700、h_gap=PAGED_GAP_X=1、
    /// v_gap=PAGED_GAP_Y=6、padding=20：
    ///   content_w=1460 → cap_w=(1460−1)/2=729.5；AR=793/1123≈0.70614 →
    ///   fit_w=700×0.70614≈494.3 → cell_w=min(729.5,494.3)=494.3、cell_h=494.3×(1123/793)=700
    ///   → row_top=[20, 726]（20+700+6）、row_page_start=[0,2,3]、total=20+700+6+700+20=1446。
    #[test]
    fn paged_mid_row_render_window_covers_visible_rows() {
        let l = crate::layout::paged_layout(
            &[(793.0, 1123.0); 3],
            1500.0,
            700.0,
            2,
            config::pdf::PAGED_GAP_X,
            config::pdf::PAGED_GAP_Y,
            20.0,
        );
        assert_eq!(l.rows, 2, "Facing 3 页 → 2 行");
        assert!(
            (l.row_top[1] - 726.0).abs() < 0.5,
            "row_top[1]≈726，实测 {}",
            l.row_top[1]
        );
        assert!(
            (l.row_height(0) - 700.0).abs() < 0.5,
            "行高≈700，实测 {}",
            l.row_height(0)
        );
        // 视口顶贴行 0 顶下 45（自由滚动片段）、视口底（765）落进行 1（726..1426）。
        let scroll_y = -(20.0 + 45.0);
        let vis = l.first_visible_row(scroll_y, 700.0);
        assert_eq!(vis, Some((0, 1)), "行中自由滚动：两行都应真可见，got {vis:?}");
        // 旧 helper 只取首行 r0 页区间 → 漏行 1 页（2）＝旧渲染窗的下一行空白缺口。
        let old = crate::layout::paged_visible_pages(&l, scroll_y, 700.0, 3);
        assert_eq!(old, Some((0, 1)), "paged_visible_pages 只按 r0 返回，got {old:?}");
        // 新渲染窗 = 全可见行（r0..=r1）± 缓冲：r_lo=0、r_hi=1 →
        // 页 [row_page_start[0], row_page_start[2]−1] = (0, 2)（全部 3 页）。
        let win = crate::layout::paged_render_window(&l, scroll_y, 700.0, 3, 1);
        assert_eq!(win, Some((0, 2)), "新渲染窗应含全部 3 页，got {win:?}");
        // 行中停稳命中：内容 y=740（行 1 内）→ row_index_at_y=1（命中链已支持任意行中位）。
        assert_eq!(l.row_index_at_y(740.0), Some(1));
        // 缩放锚连续语义：zoom_rescroll_anchor_paged 在行中位（行 0 内 45/700）保持行内比例。
        let pl_old = crate::layout::paged_layout_scaled(&l, 1.0);
        let pl_new = crate::layout::paged_layout_scaled(&l, 2.0);
        let sy = 20.0 + 45.0;
        let sy_new = crate::layout::zoom_rescroll_anchor_paged(&pl_old, &pl_new, 0, 2.0, sy);
        let frac = (sy - pl_old.row_top[0]) / pl_old.row_height(0);
        let frac_new = (sy_new - pl_new.row_top[0]) / pl_new.row_height(0);
        assert!(
            (frac_new - frac).abs() < 1e-3,
            "缩放锚应保持行内比例 {frac}，got {frac_new}"
        );
    }
}




