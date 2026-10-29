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

mod layout;
mod parser;
mod recolor;
mod view;

// `PageText`/`PdfTextChar` 为模块公开 API：非测试构建的消费方在 view 层（PageText）与
// 后续 P3（搜索，PdfTextChar 字符盒），测试亦经此引用；bin crate 无外部消费方故需 allow。
#[allow(unused_imports)]
pub use parser::{
    AnnotKind, AnnotateItem, AnnotationInfo, DocId, MemOpResult, PageText, PdfImageBox,
    PdfImagePixels, PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfTextChar, PdfWorker,
    QuadPt, RecolorEntry, RenderRequest, RenderResult, TileRequest, TileResult,
    is_writeback_busy_error,
};
pub use recolor::{RecolorCurve, RecolorParams, apply_recolor, apply_recolor_with_curve};
pub use view::PdfViewer;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use image::{Frame, RgbaImage};

// 拆 workspace 后 pdf 独立为 crate：`crate::pdf::` / `config::` /
// `ui::` / `viewer::` 短路径改为指向本 crate 根或对应 crate。
use crate as pdf;

/// 测试键位助手：pdf 独立 crate 后其测试不能再调根包的 `crate::test_support::bind_local_keymaps`
/// （组合函数含 `markdown::wysiwyg::bind_keys` 且定义在根包，会形成依赖反转）。
/// 本助手 = `base::actions::bind_input_keys`（Input 键上下文）+
/// `ui::root::bind_keys`（Root 键上下文），等价于原组合去掉 markdown 部分，
/// 覆盖 pdf 测试真正用到的两套键上下文（PdfViewer 挂在窗口 Root 下，无 WyEditor/Input）。
#[cfg(test)]
pub mod test_support;

/// 由 **紧凑 BGRA 像素字节** 直接重建 [`gpui::RenderImage`]（零像素循环，仅包一层缓冲）。
/// pdfium 渲染结果经 [`parser`] 原样带回，主线程只做 `from_raw` 包裹。
pub fn render_image_from_bgra(bgra: Vec<u8>, w: u32, h: u32) -> Result<Arc<RenderImage>> {
    let buf = RgbaImage::from_raw(w, h, bgra).context("坏像素缓冲")?;
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buf)])))
}

/// 进程单调时钟（自**首个**计时日志起算的毫秒数）：`parser`（worker 侧）与 `view`
/// （主线程侧）的所有计时日志统一用它加时间戳——便于把"任务发起 / worker 完成 /
/// 主线程收集上屏"的真实时刻精确对齐（定位"本应清晰"与"实际清晰"之间的差距）。
pub(crate) fn timing_ms() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
        * 1e3
}

/// 测试支撑（`pub` 非 `cfg(test)`：根包 `app::tests` 的 PDF 用例也要经
/// [`missing_pdf_fixture`] / [`lock`] 做 SKIP 守卫与 pdfium 串行锁，跨 crate 需可见）。
pub mod test_util {
    use std::sync::{Mutex, OnceLock};

    /// pdfium-test 全局串行锁：pdfium 是 native 库，多个 #[test] 线程并发触碰
    /// （共享单例工作线程）会触发原生竞态 → 退出码 0x80000003。所有触碰 pdfium 的测试
    /// （pdf::tests 与 app::tests 的 PDF 用例）先 lock 它，保证任意 `--test-threads`
    /// 下都稳定。生产无此问题（单 UI 线程 + 单工作线程）。
    pub fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 测试 PDF 夹具路径（仓库根 `target/`，**不入库**）：
    /// 本机 Rust 进程内 pdfium 对 `fixtures/` 下的 PDF 报 FormatError（PDF Complete 之外
    /// 风格无法解析），fixtures/ 下文件一律不用；`target/` 下三个文件生产路径可解析。
    /// 保留两元素索引形状：0=基础文本型、1=长文（≥50 页）。
    /// P4：本 crate 位于 `crates/pdf`，`CARGO_MANIFEST_DIR/target` 已变，
    /// 上溯两级回工作区根 `target/`（夹具仍在仓库根 target/ 下手动装配）。
    pub fn test_pdf_paths() -> Vec<std::path::PathBuf> {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        vec![
            manifest.join("pdf测试.pdf"),
            manifest.join("pdf测试2.pdf"),
        ]
    }

    /// 缺夹具判定：`target/pdf测试*.pdf` 任一缺失即 true。CI/装好夹具的机器返回 false，
    /// 测试真跑；本机无夹具时所有依赖真 PDF 的测试用它做「缺夹具即 SKIP」守卫（夹具
    /// 超 1MB 不入库，只能本地手动装配，故 SKIP 是常态而非异常）。
    pub fn missing_pdf_fixture() -> bool {
        !test_pdf_paths().iter().all(|p| p.exists())
    }

    /// 拷贝夹具为**可写**独立工作副本（写回类测试用）：源夹具可能带 ReadOnly 属性
    /// （如下载/标记只读的 PDF——`fs::copy` 会继承该属性），导致注释/页操作写回落盘
    /// 失败（「文件只读」）。用 `read`+`write` 重建副本（新建即可写），而非 `fs::copy`。
    pub fn copy_fixture_writable(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        let _ = std::fs::remove_file(dst);
        std::fs::write(dst, std::fs::read(src)?)?;
        Ok(())
    }
}

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
        let rx = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 });
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
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale });
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
        let rx_thumb = worker.request_thumb(doc_id, 0);
        let thumb = rx_thumb
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到缩略图")
            .expect("缩略图渲染应成功");
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 });
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
        let rx_mid = worker.request_mid(doc_id, 0, 1.0);
        let mid = rx_mid
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到中清页")
            .expect("中清页渲染应成功");
        let rx_full = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 });
        let full = rx_full
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到全清")
            .expect("全清渲染应成功");
        let rx_thumb = worker.request_thumb(doc_id, 0);
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
            let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 0.5 });
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
        let rx = worker.request_render(doc_id, RenderRequest { page: 0, scale: 0.5 });
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
        let rx_img = worker.request_render(doc_id, RenderRequest { page: page_idx, scale });
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
        let rx0 = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 });
        rx0.recv_timeout(Duration::from_secs(60))
            .expect("基准渲染应返回");
        let baseline_ms = t0.elapsed().as_secs_f64() * 1e3;

        // 触发全文档检索；30ms 后（worker 已进入检索分片）再排一张整页渲染。
        // ⚠️ 渲染到达时间必须用独立线程单独计时——不能先 recv 检索再测渲染的
        //    `elapsed()`（那会把“我读它那一刻”的墙钟也算进去，掩盖真实的到达时刻）。
        let rx_search = worker.search(doc_id, "配置".to_string());
        std::thread::sleep(Duration::from_millis(30));
        let rx_render = worker.request_render(doc_id, RenderRequest { page: 0, scale: 1.0 });
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
            let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 1.0 });
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
            let rx = worker.request_render(doc_id, RenderRequest { page: 2, scale: 1.0 });
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
        let rx = worker.request_render(doc_id, RenderRequest { page: idx, scale: 1.0 });
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
        // Ctrl 滚轮**不应滚动**：滚动偏移应恰好等于「缩放前 × 缩放比」的锚定值。
        let off1 = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).scroll.offset());
        let exp_y1 = off0.y.as_f32() * (w1 / w0);
        let exp_x1 = off0.x.as_f32() * (w1 / w0);
        assert!(
            (off1.y.as_f32() - exp_y1).abs() < 1.0 && (off1.x.as_f32() - exp_x1).abs() < 1.0,
            "Ctrl 放大不应产生额外滚动：锚定 y={exp_y1:.1} 实际 y={:.1}（x 锚={exp_x1:.1} 实际 x={:.1}）",
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
        cx.simulate_event(wheel(true, -240.0));
        for _ in 0..4 {
            cx.run_until_parked();
        }
        let (w3, z3, off3) = cx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.page_width(), v.zoom, v.scroll.offset())
        });
        assert!(w3 < w1, "Ctrl+向下滚应缩小：{w1} → {w3}");
        assert!(z3 < z1, "zoom 应变小：{z1} → {z3}");
        let exp_y3 = off2.y.as_f32() * (w3 / w1);
        let exp_x3 = off2.x.as_f32() * (w3 / w1);
        assert!(
            (off3.y.as_f32() - exp_y3).abs() < 1.0 && (off3.x.as_f32() - exp_x3).abs() < 1.0,
            "Ctrl 缩小也不应滚动：锚定 y={exp_y3:.1} 实际 y={:.1}",
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
    /// 带时间戳（`timing_ms`）的巡检行，与 `GPUI_EDITOR_PDF_TIMING=1` 的控制台日志对位。
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
            // 测试窗口为 1× DPI（gpui TestAppContext 默认），缩放因子即 1.0
            let sf = 1.0f32;
            let mut covers_held = 0usize;
            let mut mids_held = 0usize;
            let mut thumbs_held = 0usize;
            let mut double_hold = 0usize;
            let mut cover_bytes = 0u64;
            for (p, s) in v.slots.iter().enumerate() {
                if s.cover.is_some() {
                    covers_held += 1;
                    let (w, h) =
                        layout::page_bitmap_size(v.dims[p], layout::cover_scale(v.dims[p], pw, sf));
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
        let cap = config::pdf::TILE_CACHE_BYTES;
        eprintln!(
            "[MEM zoom=1.0] tile_bytes={}MB covers={} mids={} thumbs={} double_hold={} est_total={}MB cap={}MB",
            tile_bytes / 1024 / 1024,
            covers,
            mids,
            thumbs,
            double_hold,
            total_est / 1024 / 1024,
            cap / 1024 / 1024,
        );
        assert!(tile_bytes <= cap, "zoom1 瓦片占用应不超预算：{tile_bytes} > {cap}");
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
        assert!(
            total_est <= 69 * 1024 * 1024 * 120 / 100,
            "zoom1 est_total 不应超 8bab7be 基线（~69MB）120%：{total_est}"
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
        eprintln!(
            "[MEM zoom=1.75(d=2.47)] tile_bytes={}MB covers={} mids={} thumbs={} double_hold={} est_total={}MB cap={}MB",
            tile_bytes2 / 1024 / 1024,
            covers2,
            mids2,
            thumbs2,
            double_hold2,
            total_est2 / 1024 / 1024,
            cap / 1024 / 1024,
        );
        assert!(
            tile_bytes2 <= cap,
            "zoom1.75 瓦片占用应不超预算：{tile_bytes2} > {cap}"
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
        assert!(
            total_est2 <= 108 * 1024 * 1024 * 120 / 100,
            "zoom1.75 est_total 不应超 8bab7be 基线（~108MB）120%：{total_est2}"
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
            // 封面先行主路径：从首屏停稳到远跳翻页停稳，瓦片系派发增量为 0（旧语义这次
            // 翻页会补一屏瓦片 → `tile_reqs` 增长 = 双渲回归）；目标页由封面供给、同页无瓦片。
            // 注：`tile_reqs` 非零部分来自开档初始最大化窗（页宽使密度 > COVER_SCALE_CAP
            // 的「底图+精修」合法瓦片，与本「同密度封面 vs 瓦片」回归口径无关）。
            assert_eq!(t1, t0, "封面 affordable 首屏/翻页不应派发新增瓦片栅格命令");
            assert!(cover_present, "封面 affordable 目标页应已由封面供给");
            assert!(!tiles_on_page, "封面 affordable 目标页不得同页持瓦片（无双渲）");
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
    /// **不调用 apply_recolor**——`set_recolor(Some(night))` 的渲染结果与 `None`（未着色
    /// 原图）逐字节一致（CPU 税归零）；门控关闭时恢复既有 CPU 着色（`Some` ≠ `None`）。
    /// 用「同页同密度渲染对比」作可计数/可字节断言：门控开启的着色档输出 == 原图基准；
    /// 门控关闭的着色档输出 != 原图基准（与 dbd3236 行为一致）。
    #[test]
    fn gpu_gate_worker_skips_cpu_recolor_and_off_keeps_it() {
        skip_if_missing_pdf_fixture!();
        let _guard = pdf_lock().lock().unwrap();
        // 进程级门控是 worker/view 共读的全局开关：本仓库所有触碰 worker/view 的测试都持
        // pdfium-test 全局锁，故进程内瞬时一致；用 RAII 守卫，断言失败也会还原。
        let _gate = GpuGateGuard::set_on();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let night = RecolorParams {
            bg: [0x20, 0x20, 0x20],
            ink: Some([0xD8, 0xD8, 0xD8]),
        };
        // 原图基准（未着色）：门控开启下取 None 档，两次应逐字节一致。
        worker.set_recolor(doc_id, None);
        let rx_raw = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 });
        let raw = rx_raw
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("原图应返回")
            .expect("渲染应成功");
        // 门控开启：设了 night 但显示像素仍是未着色原图（worker 跳过 CPU 着色）。
        worker.set_recolor(doc_id, Some(night));
        let rx_gpu = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 });
        let gpu_out = rx_gpu
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("门控开启着色档应返回")
            .expect("渲染应成功");
        assert!(
            gpu_out.bgra == raw.bgra,
            "门控开启时 recolor 不应改变显示像素（worker 跳过 CPU 着色）：{}",
            "亮/暗像素应保持未着色原图"
        );
        // 内存去冗余（本线目标）：GPU 门控默认路径 worker 不写 raw 缓存（0 消费者，view
        // 持原图、着色在绘制时 img().recolor 现算），缓存长度为 0；出口 move 直传原图
        // （返回位图 == 原图，上文已字节等值断言）。
        assert_eq!(
            worker.raw_cache_len(doc_id),
            0,
            "GPU 门控路径不应驻留 worker raw 缓存（0 消费者，默认缓存冗余归零）"
        );

        // 门控关闭：恢复既有 CPU 着色（回归不变式，与 dbd3236 一致）。
        crate::recolor::set_gpu_recolor_gate_for_test(false);
        worker.set_recolor(doc_id, Some(night));
        let rx_cpu = worker.request_render(doc_id, RenderRequest { page: 0, scale: 2.0 });
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
    /// 接线成立；切档不派新栅格（`raster_reqs` 不变），故「切档 0 重算 0 重栅」成立。
    /// GPU 像素无法 headless 断言（K23②），此处只证「机制 + 门控」（真机复核像素等价）。
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

        // 切到夜间档（GPU 通道：只改图片参数、0 重算 0 重栅）。
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
        // 泵/帧推进：GPU 通道无 pending_recolor（不派重着色刷新），只需驱动一帧重绘。
        for _ in 0..60 {
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(2));
        }
        let req1 = cx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().read(cx).raster_reqs
        });
        assert_eq!(req1, req0, "门控开启切档应 0 新栅格命令（GPU 参数即开关）");

        // 场景断言：页卡/瓦片 img 携带了重着色参数（RecolorSprite 图元，bg/switches 正确）。
        let sprites = cx.update(|window, _| window.painted_recolor_sprites());
        assert!(
            !sprites.is_empty(),
            "门控开启 + 夜间档应产生 RecolorSprite 场景图元（PDF 显示 img 挂上重着色参数）"
        );
        // bg 归一化 0x20/255 ≈ 0.1255；switches.x=1 启用墨带；ink 0xD8/255 ≈ 0.8471；
        // switches.y=1 = FullBand（曲线 latch 默认/本测试显式设为 FullBand）——shader 分支依据。
        let bg0 = 0x20u8 as f32 / 255.0;
        let ink0 = 0xD8u8 as f32 / 255.0;
        let ok = sprites.iter().any(|s| {
            s.switches[0] == 1.0
                && s.switches[1] == 1.0
                && (s.bg[0] - bg0).abs() < 1e-3
                && (s.ink[0] - ink0).abs() < 1e-3
        });
        assert!(
            ok,
            "至少一个 RecolorSprite 应携带正确的纸/墨参数 + FullBand 曲线（ink 启用、switches.y=1、bg=0x20、ink=0xD8）"
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
        let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
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
        let rx = wa.request_render(da, RenderRequest { page: 0, scale: 1.0 });
        let before = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("收到渲染")
            .expect("渲染成功");
        wa.close(da);

        // 从磁盘重读字节 → 重开（效果断言）
        let (w2, d2, dims2) =
            PdfWorker::open_and_dims(std::fs::read(&path).expect("改后重读")).expect("重开改后文件");
        assert_eq!(dims2, dims, "写回注释不应改变页数与页尺寸");
        let rx2 = w2.request_render(d2, RenderRequest { page: 0, scale: 1.0 });
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
            let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
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
            let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
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
        let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
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
        let vid = viewer.clone();
        assert!(
            wait(cx, &mut |cx| cx.update(|app| {
                let v = vid.read(app);
                v.is_loaded()
                    && v.annotations_for_page(0)
                        .map(|a| a.iter().any(|x| x.kind == AnnotKind::Highlight))
                        .unwrap_or(false)
            })),
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
}




