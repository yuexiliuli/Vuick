//! PDF 栅格化**核心层**（pdfium 定位/单例工作线程/逐页逐瓦片栅格化 + 页列纯布局几何 +
//! 纸墨重着色），零视图/渲染依赖。
//!
//! 本 crate 与 `pdf`（视图层）同源拆出（镜像 `editor_core`/`editor`）：`pdf` 保留包名并以
//! `pub use pdf_core::*` 全量重导出本层，外部代码继续经 `pdf::parser::X` / `pdf::layout::X` /
//! `pdf::recolor::X` 等旧路径访问，消费者 crate（shell / config）零改动。
//!
//! # 边界（与视图层的职责划分）
//!
//! - 本层只维护栅格化/几何/重着色：`parser` = pdfium dll 定位 + 进程级单例工作线程 +
//!   多文档逐页/逐瓦片栅格化（RGBA）；`layout` = 页列纯布局 + 瓦片网格几何（worker 与 view
//!   共用同一套数学）；`recolor` = 纸墨重着色（CPU LUT + GPU 门控）。
//! - 显示、滚动调度、瓦片按需渲染、交互、渲染全部在 `pdf` 视图层。

#![allow(dead_code)] // 组件库公共 API：跨 crate 重导出面，未直接使用项不误报 dead_code

pub mod layout;
pub mod parser;
pub mod recolor;

// `PageText`/`PdfTextChar` 为模块公开 API：非测试构建的消费方在 view 层（PageText）与
// 后续 P3（搜索，PdfTextChar 字符盒），测试亦经此引用；bin crate 无外部消费方故需 allow。
#[allow(unused_imports)]
pub use parser::{
    AnnotKind, AnnotateItem, AnnotationInfo, DocId, MemOpResult, PageText, PdfImageBox,
    PdfImagePixels, PdfOutlineEntry, PdfSearchHit, PdfSearchResults, PdfTextChar, PdfWorker,
    QuadPt, RecolorEntry, RenderRequest, RenderResult, TileRequest, TileResult,
    is_writeback_busy_error,
};
#[allow(unused_imports)]
pub use recolor::{RecolorCurve, RecolorParams, apply_recolor, apply_recolor_with_curve};

use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::RenderImage;
use kimg::{Frame, RgbaImage};

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与 `editor_core` 拆 crate 同一策略）：让本 crate 内部既有的
// `crate::pdf::`（自引用）旧路径在拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：自引用。本 crate 根 = 原 `pdf` crate 的模块根，`crate::pdf::X` → `crate::X`。
use crate as pdf;

/// 由 **紧凑 BGRA 像素字节** 直接重建 [`gpui::RenderImage`]（零像素循环，仅包一层缓冲）。
/// pdfium 渲染结果经 [`parser`] 原样带回，主线程只做 `from_raw` 包裹。
pub fn render_image_from_bgra(bgra: Vec<u8>, w: u32, h: u32) -> Result<Arc<RenderImage>> {
    let buf = RgbaImage::from_raw(w, h, bgra).context("坏像素缓冲")?;
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buf)])))
}

/// 由 **R8 单通道感知亮度字节**（`w*h*1`）重建 [`gpui::RenderImage`]（P2：瓦片/封面 R8
/// 通道）。上传走 Monochrome atlas（1BPP，glyph 同款 `DXGI_FORMAT_R8_UNORM`），GPU shader
/// 按 R8 读取 —— 存储与传输 -4×。
pub fn render_image_from_luma(luma: Vec<u8>, w: u32, h: u32) -> Result<Arc<RenderImage>> {
    Ok(Arc::new(RenderImage::new_luma(vec![gpui::LumaFrame::new(
        luma, w, h,
    )])))
}

/// 按位图数据布局（BGRA 4BPP / R8 luma 1BPP，由 worker 侧 `luma` 标志标定）选择包装路径
///（view 侧回收结果统一入口）。`luma == true` → 1BPP 亮度包装；否则既有 BGRA 包装。
/// 跨 crate 可见性：`pdf` 视图层（`view/tiles.rs` 等）直接消费，故由原 `pub(crate)` 升 `pub`。
pub fn wrap_render_image(
    bytes: Vec<u8>,
    w: u32,
    h: u32,
    luma: bool,
) -> Result<Arc<RenderImage>> {
    if luma {
        render_image_from_luma(bytes, w, h)
    } else {
        render_image_from_bgra(bytes, w, h)
    }
}

/// 进程单调时钟（自**首个**计时日志起算的毫秒数）：`parser`（worker 侧）与 `view`
/// （主线程侧）的所有计时日志统一用它加时间戳——便于把"任务发起 / worker 完成 /
/// 主线程收集上屏"的真实时刻精确对齐（定位"本应清晰"与"实际清晰"之间的差距）。
/// 跨 crate 可见性：`pdf` 视图层（`view/tiles.rs` 等）直接消费，故由原 `pub(crate)` 升 `pub`。
pub fn timing_ms() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
        * 1e3
}

/// 测试支撑（`pub` 非 `cfg(test)`：主 crate `app::tests` 的 PDF 用例也要经
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
    /// P4：本 crate 位于 `crates/pdf_core`，`CARGO_MANIFEST_DIR/target` 已变，
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
