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
//!
//! 书签目录统一由**侧边栏**提供（见 [`crate::app::sidebar`] 与公共 [`crate::toc`]），
//! 本模块只保留书签数据（[`PdfOutlineEntry`]）与跳页能力（[`crate::pdf::PdfViewer::scroll_to_page`]）。

mod cache;
mod layout;
mod parser;
mod view;

// `PageText`/`PdfTextChar` 为模块公开 API：非测试构建的消费方在 view 层（PageText）与
// 后续 P3（搜索，PdfTextChar 字符盒），测试亦经此引用；bin crate 无外部消费方故需 allow。
#[allow(unused_imports)]
pub use parser::{
    DocId, PageText, PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfTextChar, PdfWorker,
    RenderRequest, RenderResult,
};
pub use view::PdfViewer;

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
            manifest.join("pdf测试.pdf"),
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
    fn worker_renders_thumbnail_at_low_resolution() {
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
        let expect_w = ((crate::config::pdf::THUMB_WIDTH / pt_w.max(1.0))
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
        let _guard = pdf_lock().lock().unwrap();
        // L0.5 中清页：宽度应按 MID_WIDTH 推导，体积介于缩略图与全清之间（拖动清晰度主力）
        let p = test_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx_mid = worker.request_mid(doc_id, 0);
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
        let expect_w = ((crate::config::pdf::MID_WIDTH / pt_w.max(1.0))
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

    /// 文本层坐标映射回归（P1 核心验收）：字符盒必须命中渲染位图上的字形墨迹，
    /// 且 Y 朝向正确。若翻转方向写反 / 映射比例错，盒会落到空白处（无墨）或上下颠倒。
    #[test]
    fn text_boxes_align_with_rendered_glyphs() {
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let display_w = crate::config::pdf::PAGE_WIDTH;

        // 找一张内容稠密页（≥40 个可测字符），避免封面/空白页样本量不足
        let max_scan = dims.len().min(10);
        let mut chosen: Option<(usize, PageText, (f32, f32), f32)> = None;
        for page_idx in 0..max_scan {
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
                let page_pt = dims[page_idx];
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
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let page_idx = 1usize;
        let page_pt = dims[page_idx];
        let display_w = crate::config::pdf::PAGE_WIDTH;
        let scale = layout::render_scale(display_w, 1.0, page_pt.0);
        let rx_text = worker.request_text(doc_id, page_idx);
        let text = rx_text
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到文本")
            .expect("文本应成功");

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
        let _guard = pdf_lock().lock().unwrap();
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/pdf测试.pdf");
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
            if best.as_ref().map_or(true, |(_, t)| {
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
        eprintln!("pdf测试.pdf 第 {} 页首行复制样本: {:?}", page_idx + 1, &one);

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
        let _guard = pdf_lock().lock().unwrap();
        let p = test_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");

        let rx = worker.search(doc_id, "空调".to_string());
        let results = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("应收到检索结果")
            .expect("检索应成功");
        assert!(
            !results.hits.is_empty(),
            "pdf测试.pdf 应能检索到「空调」以外的正文词"
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
            assert_eq!(seg, "空调", "命中文本应与关键字一致，实际 {seg:?}");
            assert!(
                hit.snippet.contains("空调"),
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

    /// 回归：PDF 页列必须以「滚动可视区」为基准水平居中，而不是全窗口。
    /// 侧边栏/分屏开启时可视区窄于窗口，若按窗口宽居中会整体右偏；
    /// 同时可视区窄于「页宽+边距」时必须产生横向溢出（可滚到全部页面）。
    #[gpui::test]
    async fn pdf_column_centers_within_pane_width(cx: &mut gpui::TestAppContext) {
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
        cx.update(gpui_component::init);

        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/pdf测试.pdf");
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
            if pane_w > 840.0 + 2.0 * crate::config::pdf::PAGE_PAD_X {
                let right_pad = (scroll.right() - pages.right()).as_f32();
                let center_off = (pages.center().x - scroll.center().x).as_f32();
                assert!(
                    center_off.abs() < 1.0,
                    "宽区页列应居中：center_off={center_off:.3}px（left={left_pad:.1} right={right_pad:.1}）"
                );
            } else {
                assert!(
                    (left_pad - crate::config::pdf::PAGE_PAD_X).abs() < 1.0,
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
        cx.update(gpui_component::init);
        let pdf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/pdf测试.pdf");
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
}
