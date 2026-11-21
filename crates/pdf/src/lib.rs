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
mod tests;
