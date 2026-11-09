//! `wy_math_render`：markdown-wysiwyg 的「数学渲染」子系统（独立 crate）。
//!
//! 原居 `markdown-wysiwyg` 的 `wysiwyg/math/`（`math/mod.rs` 行内 `$…$` / 块 `$$…$$`
//! 公式整形 + `math_typeset.rs` LaTeX→`RenderImage` 排版）与 `wysiwyg/types.rs` /
//! `wysiwyg/element/geom.rs` 的随迁项（`MathSlot` / `inline_math_key` /
//! `display_to_source`），随数学专项拆出。
//! host（markdown-wysiwyg）经 `pub use wy_math_render::{…}` /
//! `pub(crate) use wy_math_render::*` 重导出到上层命名空间，既有 `use super::*`
//! 消费方（shape/frame/paint/tests）代码路径不变。
//!
//! 与本 crate 的依赖方向一致地避开 host：**本 crate 不反向依赖 markdown-wysiwyg**。
//! host↔本 crate 的解耦面只有两处注入（`splice_math_text`）：
//! ① 行首偏移 `line_start`（host 调用点先算 `editor.line_start(src_line)` 传入）；
//! ② 排版槽就绪查询闭包 `slot_ready`（host 的 `editor.math_slots()` 经闭包注入，
//! `MathSlot::Ready` → 提取位图 + 逻辑 px 尺寸，`Loading`/`Failed`/缺位 → `None`）。

mod math;
mod math_typeset;

// 跨 crate 消费的入口（原 math 模块公有的全部 + 从 host 随迁项）：
// `splice_math_text` 行内公式 splice、`math_to_unicode`/`math_to_segments` Unicode
// 展平、`inline_math_fit*` 行内缩放、`inline_math_key` 行内槽键、`MathSlot` 槽状态、
// `InlineMathText` spacer 描述（Frame 携带）、`display_to_source` 渲染列↔源列纯函数；
// `render_latex_with_pad`/`Rendered` LaTeX→位图排版应在 host `content_sync` 消费。
pub use math::{
    InlineMathText, MathPos, MathSeg, MathSlot, display_to_source, inline_math_fit,
    inline_math_fit_h, inline_math_key, math_to_segments, math_to_unicode, splice_math_text,
    visual_block_line_for_index,
};
pub use math_typeset::{Rendered, render_latex_with_pad};
