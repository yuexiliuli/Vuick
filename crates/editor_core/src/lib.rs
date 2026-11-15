//! 自研文本/代码编辑器的**核心模型层**：buffer/selection/undo/movement/rope_ext/
//! highlight/fold/markdown/find/goto（纯状态与纯函数，零视图/渲染依赖）。
//!
//! 本 crate 与 `editor`（视图层）同源拆出：`editor` 保留包名并以其 `pub use editor_core::*`
//! 全量重导出本层，外部代码继续经 `editor::buffer::X` / `editor::movement::X` 等旧路径
//! 访问，消费者 crate（shell / markdown-wysiwyg / ui）零改动。
//!
//! # 边界（与视图层的职责划分）
//!
//! - 本层只维护文档状态与查询：字节↔行/列双射、选区、光标移动、撤销/重做、语法高亮
//!   缓存、折叠映射、Markdown 装饰滤镜、查找/跳转状态机——全是可脱离 GPU/窗口运行的
//!   纯模型；
//! - 输入框实体、防抖调度、卡片绘制、滚动落地、命中/渲染全部在 `editor` 视图层。

#![allow(dead_code)] // 组件库公共 API：跨 crate 重导出面，未直接使用项不误报 dead_code

pub mod buffer;
pub mod caret;
pub mod find;
pub mod fold;
pub mod goto;
pub mod highlight;
pub mod markdown;
pub mod movement;
pub mod rope_ext;
pub mod selection;
pub mod undo;

#[allow(unused_imports)]
pub use buffer::{Buffer, Edit};
#[allow(unused_imports)]
pub use selection::Selection;
#[allow(unused_imports)]
pub use undo::{EditIntent, Transaction, UndoManager};
#[allow(unused_imports)]
pub use highlight::Highlighter;
#[allow(unused_imports)]
pub use fold::{Fold, FoldMap};
#[allow(unused_imports)]
pub use find::FindOverlay;
#[allow(unused_imports)]
pub use goto::{GoToOverlay, GotoTarget, parse_and_clamp, parse_goto};
#[allow(unused_imports)]
pub use rope_ext::{Bias, Point, RopeExt};
#[allow(unused_imports)]
pub use markdown::MarkdownDecor;

// ---------------------------------------------------------------------------
// 迁移期 crate-root 短别名（与 `editor` 拆 crate 同一策略）：
// 让本 crate 内部既有的 `crate::editor::`（自引用）/ `crate::prof::` 旧路径在
// 拆 crate 后零改动即可解析。
// ---------------------------------------------------------------------------
/// 迁移期别名：自引用。本 crate 根 = 原 bin 里 `editor` 模块，`crate::editor::X` → `crate::X`。
use crate as editor;
/// 迁移期别名：`crate::prof::` → `base::prof`（highlight 的耗时探针别名）。
use base::prof;
