//! PDF 页面的**纯布局/调度数学**：页列尺寸、可见页区间、渲染倍率、瓦片网格、每帧工作规划。
//!
//! 全部为无副作用纯函数（便于单测），与 `view.rs` 的页列 DOM 绝对定位**使用同一套数学**，
//! 保证滚动位置推算与 DOM 位置严格一致（否则可见页会被误判）。瓦片几何（[`tile_rect`]）
//! 由 **worker 线程**（`parser.rs`）与 **view 线程**（`view.rs`）共用——两侧各自独立算出
//! 的瓦片矩形必须逐像素一致，拼片才不会错位。
mod grid;
mod paged;
mod single;
mod text;

pub use grid::*;
pub use paged::*;
pub use single::*;
pub use text::*;

#[cfg(test)]
mod tests;
