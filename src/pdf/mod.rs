//! PDF 读取与展示模块。
//!
//! 基于 native **pdfium**（Edge 同源内核）栅格化，单页渲染快，首次浏览即流畅。
//! 架构：页级虚拟化 + 内存预算 + 磁盘二级缓存：
//! - [`PdfWorker`]（`parser.rs`）是**进程级单例后台线程**（pdfium 整进程只能绑定一次），
//!   持有唯一 `Pdfium` 与一个 `HashMap<DocId, PdfDocument>`，通过 mpsc 接收各阅读器的
//!   `Open / Render / Close` 请求并回传 **BGRA 像素**；
//! - [`PdfViewer`]（`view.rs`）只做收发与位图显示：首帧用 BGRA 直接包 `RenderImage`
//!   （主线程零像素循环，流畅），翻回已淘汰页时从磁盘缓存恢复。
//!
//! 模块结构：
//! - [`parser`]：pdfium.dll 定位、单例工作线程、多文档逐页栅格化（BGRA）
//! - [`view`]：阅读器视图核心（加载/滚动/按需渲染/滚动条）
//! - [`layout`]：页列纯布局/调度数学（尺寸、可见区间、渲染规划）
//! - [`cache`]：磁盘二级页缓存（紧凑 BGRA 落盘 + LRU 封顶）
//! - [`toc`]：书签目录面板（折叠、悬浮、拖拽调宽）与其交互

mod cache;
mod layout;
mod parser;
mod toc;
mod view;

pub use parser::{DocId, PdfOutlineEntry, PdfWorker, RenderRequest, RenderResult};
pub use view::PdfViewer;

// 目录折叠/可见性的纯函数（供 EPUB 等其它阅读器复用同一套树可见性逻辑）
pub(crate) use toc::{toc_node_has_child, toc_visibility};

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use image::{Frame, RgbaImage};

/// 由 **紧凑 BGRA 像素字节** 直接重建 [`gpui::RenderImage`]（零像素循环，仅包一层缓冲）。
/// pdfium 渲染结果经 [`parser`] 原样带回，主线程只做 `from_raw` 包裹。
pub fn render_image_from_bgra(bgra: Vec<u8>, w: u32, h: u32) -> Result<Arc<RenderImage>> {
    let buf = RgbaImage::from_raw(w, h, bgra).context("坏像素缓冲")?;
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buf)])))
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::{Mutex, OnceLock};

    /// pdfium-test 全局串行锁：pdfium 是 native 库，多个 #[test] 线程并发触碰
    /// （共享单例工作线程）会触发原生竞态 → 退出码 0x80000003。所有触碰 pdfium 的测试
    /// （pdf::tests 与 app::tests 的 PDF 用例）先 lock 它，保证任意 `--test-threads`
    /// 下都稳定。生产无此问题（单 UI 线程 + 单工作线程）。
    pub fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
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
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        vec![
            manifest.join("N107内外饰配置表NEW.pdf"),
            manifest.join("17. 道路机动车辆产品准入审查要求1.0.pdf.pdf"),
        ]
    }

    #[test]
    fn parses_all_test_pdfs_and_reads_dims() {
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
        let entries = vec![
            crate::pdf::PdfOutlineEntry {
                title: "章节一".into(),
                level: 0,
                page_index: 0,
            },
            crate::pdf::PdfOutlineEntry {
                title: "子节".into(),
                level: 1,
                page_index: 1,
            },
        ];
        assert!(entries.iter().all(|e| !e.title.trim().is_empty() && e.level <= 6));
    }

    #[test]
    fn worker_renders_single_page_to_bgra() {
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

    #[test]
    fn worker_renders_many_pages_individually() {
        let _guard = pdf_lock().lock().unwrap();
        // 259 页的 PDF：抽查首/中/末各一页（模拟「滚动到哪渲染哪」）
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let dims = parser::page_dims(&bytes).expect("解析应成功");
        assert!(dims.len() >= 200, "长文档应有大量页，实际={}", dims.len());
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
            for px in bgra.chunks_exact(4) {
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

    #[test]
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
}
