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
mod view;

// `PageText`/`PdfTextChar` 为模块公开 API：非测试构建的消费方在 view 层（PageText）与
// 后续 P3（搜索，PdfTextChar 字符盒），测试亦经此引用；bin crate 无外部消费方故需 allow。
#[allow(unused_imports)]
pub use parser::{
    DocId, PageText, PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfTextChar, PdfWorker,
    RenderRequest, RenderResult, TileRequest, TileResult,
};
pub use view::PdfViewer;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use raster::{Frame, RgbaImage};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本模块 pdfium 测试的串行锁（实现见 [`test_util::lock`]）。
    fn pdf_lock() -> &'static std::sync::Mutex<()> {
        test_util::lock()
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
            let scroll = cx.debug_bounds("pdf-scroll").expect("应有滚动容器");
            let pages = cx
                .debug_bounds("pdf-pages")
                .expect("应有页列（宽=页宽）");
            assert_eq!(scroll.size.width.as_f32(), pane_w, "滚动可视区宽应为 pane 宽");
            let left_pad = (pages.left() - scroll.left()).as_f32();
            if pane_w > 840.0 + 2.0 * config::pdf::PAGE_PAD_X {
                let right_pad = (scroll.right() - pages.right()).as_f32();
                let center_off = (pages.center().x - scroll.center().x).as_f32();
                assert!(
                    center_off.abs() < 1.0,
                    "宽区页列应居中：center_off={center_off:.3}px（left={left_pad:.1} right={right_pad:.1}）"
                );
            } else {
                assert!(
                    (left_pad - config::pdf::PAGE_PAD_X).abs() < 1.0,
                    "窄区页面色左对齐加 PAGE_PAD_X 边距：left_pad={left_pad:.1}"
                );
                assert!(
                    pages.right().as_f32() > scroll.right().as_f32() + 1.0,
                    "窄区页面应横向溢出以触发横向滚动"
                );
            }
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
        assert!((w0 - 840.0).abs() < 1.0, "初始页宽应为 PAGE_WIDTH");
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
                "第 {target} 页停稳后仅保留当前窗口±1 封面（{covers_held} > 5 = 封面在累积）"
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
        assert!(covers <= 5, "zoom1 封面驻留应 ≤ 5：{covers}");
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
        assert!(covers2 <= 5, "zoom1.75 封面驻留应 ≤ 5：{covers2}");
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
}
