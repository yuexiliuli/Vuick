// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Vuick contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! 本项目管线实现的 WYSIWYG 编辑器——`markdown::wysiwyg`。
//!
//! 以本项目自研管线（`parser`/`cells`/`ir`/`doc`）实现所见即所得编辑，
//! 已取代 zorite 移植的 `markdown::live`（该模块已移除；其共享的
//! `recognition` / `inline_style` 识别层上移到 `markdown/` 顶层继续共用）。
//!
//! 本文件是第一步「初步实现」：
//! - **文档模型完全来自本项目** [`super::doc::Doc`]：IR 语义 + `BlockIndex` + 源；
//!   行物化 `rows_in_block_window` + 编辑映射 [`DocRow::byte_map`]（与预览逐字节
//!   一致，`parity` 钉死）。
//! - 编辑器 = auto-grow 自绘 [`element::WyEditorElement`]（宿主滚动，与 `live` 同宿主
//!   范式）；行内样式来自 `DocRow.runs`（`parse::RunKind`）。
//! - 非文本块已按类型渲染：代码围栏（等宽 + 语法高亮 + 圆角面板，**内容行可编辑**）、
//!   GFM 表格（网格，列宽自适应）、引述（灰底 + 左侧指示条）、独立图片行
//!   （`AnyImageCache` 异步加载，占位/失败回退 alt）；样式统一抄自预览
//!   [`super::view::style::StyleParams`]。
//! - 输入：普通文本/IME 组合命中（`marked` 区间追踪，修复重复串）、点击定位、
//!   鼠标拖选 + Ctrl+C/X/V/A（选区替换/删除）。
//! - 现状声明：代码块内容行、表格**单元格**、图片**题注（alt）**已可编辑（代码走块级
//!   `byte_map` + `multi_offsets`；表格走逐行逐格的 `DocRow.table_rows` → `table_edit`
//!   单元格排版，含对齐位移、渲染列↔源字节映射；图片行 = 图片盒 + 居中可编辑 alt
//!   题注行，题注走与普通文本行同构的 `byte_map`/`wrapped`/`x_off`；导航跳过只读
//!   围栏/分隔行，退格/删除有代码围栏、单元格、alt 起点/尾边界守卫，表格与图片
//!   alt 内回车 no-op）。选区面板高亮已覆盖：普通文本行、代码内容行、表格单元格
//!   （逐可见行 × 逐格与选区求交后按字形几何绘制）、图片题注行。仅作「预览编辑
//!   测试」入口，验证「本项目流程能编辑 + 正确映射回源」。
//!
//! **#4 统一文档层（与预览合并）**：预览与 WYSIWYG 将合为一个组件（`editable` 门控
//! 只读/可编辑，同一 `Doc` + 自绘管线）。跨管线基准（`bench_preview_vs_wy_readonly`）
//! 已验证自绘滚动带 3.67ms/步 vs 预览元素树 6.71ms/步（release，真实 6.4MB 文档）——
//! 合并方向滚动不劣反优。预览独有能力正逐个搬入：已搬**搜索跳转高亮**
//! （[`Self::highlight_search`]，paint 复用选区几何画命中色；app 侧边栏搜索在统一
//! 组件在场时同步高亮）。待搬：EPUB 阅读布局（标题居中/首行缩进/注记悬浮卡）。

/// `cache`（引擎渲染面）已迁 `markdown_wysiwyg_engine::cache`（git mv 自本文件）：
/// 行整形缓存（`RowShapeCache`）/ `.notdef` 自愈状态机（`NotdefRetry`）/ LRU 泛型核心
/// （`LruCache`/`CacheWrite`/`identity_key`）/ 布局代数（`height_epoch`/
/// `zoom_band_for_top`）现住引擎——此处**别名 re-export** 保持 `wysiwyg::cache::X` /
/// `self::cache::{…}` 既有路径原样；宿主自留的 `MathSlotCache`/`CodeParseCache`
/// （依赖 `MathSlot`/`CodeParseKey`）迁入下方 `cache_host`，经 `wysiwyg::` 重导出。
pub(crate) use markdown_wysiwyg_engine::cache;
mod cache_host;
mod code_parse;
mod content_sync;
mod core;
mod edit;
mod image;
mod math_edit;
mod mouse;
/// `prewarm`（跨帧预热状态 `Prewarm`/`PrewarmSig`）已迁 `markdown_wysiwyg_engine::prewarm`：
/// 别名 re-export 保持 `wysiwyg::prewarm::{Prewarm, PrewarmSig}` 原路径
/// （`frame/prewarm_band`、`settle_giant_blocks` 消费）。
pub(crate) use markdown_wysiwyg_engine::prewarm;
/// 三段横向滚动条（代码块 / 表格 / 数学卡）的统一 kind —— P3 / Slice C：枚举上移引擎
/// `markdown_wysiwyg_engine::model`（连同 `HScrollState` 状态桶收敛），本文件 re-export
/// 供 `view.rs`/`mouse.rs` 经 `super::*` 使用。
pub(crate) use markdown_wysiwyg_engine::model::HScrollKind;
/// `theme` 已随「元素引擎纯类型核心」拆为独立 crate `markdown_wysiwyg_engine`
/// （`git mv` 迁出 `wysiwyg/theme.rs`）：此处以内联 shim 模块**重导出**引擎 `theme::*`，
/// 下方 `self::theme::WyTheme` / `self::theme::{CaretVis, caret_vis, math_color,
/// set_caret_vis}` 等既有重导出路径原样不变；`impl From<&WyTheme> for MmTheme`
/// 已随 `WyTheme` 迁入引擎 `theme.rs`（引擎依赖 mermaid，宿主侧删除）。
mod theme {
    pub use markdown_wysiwyg_engine::theme::*;
}
mod types;
mod view;
mod warm;

/// `impl WyRenderHost for WyEditor`：宿主的渲染管线只读态面（引擎 `shape`/`frame`/
/// `paint`/`hit` 迁移后经 `&dyn ::markdown_wysiwyg_engine::WyRenderHost` 取宿主态）。
mod host_impl;

mod element;
/// `frame`：分带帧构建（`wysiwyg` 元素层核心管线）。原 `pub(crate) mod frame` 转 pub，
/// 供 shell/app 跨 crate 消费（`markdown::wysiwyg::frame::build_prebuild_pass1`）。
pub mod frame;
mod input;
/// 数学渲染已随专项拆为独立 crate `markdown_wysiwyg_math`（`wysiwyg/math` + `math_typeset.rs`
/// + `types`/`geom` 随迁项）；下方 `pub use markdown_wysiwyg_math::{…}` 在 `wysiwyg::` 下
/// 保留原 `math` 重导出面。
/// `markdown_wysiwyg_mermaid`：图卡片解析/布局/绘制（自 `wysiwyg/mermaid/` 拆为独立 crate）；
/// 布局结果类型经下方 `pub use markdown_wysiwyg_mermaid::{…}` 在 `wysiwyg::` 下可遍历。
pub(crate) mod shape;

// 子模块扁平重导出：`element::*` 把布局类型 + geom/math/shape 的 pub(crate) 项一并带进
// mod.rs 命名空间，frame/shape/math 以 `use super::*` 共享（对应 `element::*` 路径原样保留）。
// frame 不整块重导出（其内部 `warm_emoji_rasterizer` 与根级同名函数冲突），由 element.rs
// 经 `use super::frame::{…}` 精确暴露。
pub(crate) use element::*;
// 显式 pub 重导出（遮蔽 element::* 全局的同名 pub(crate) 项）：`build_frame` / `NoteMarkerHit`
// 供 shell/app 测试跨 crate 消费。
pub use self::frame::build_frame;
pub use self::element::NoteMarkerHit;

// —— M2.1 拆出的子模块项按原可见性扁平重导出（外部/兄弟对 `wysiwyg::…` 的引用不变）——
pub use self::theme::WyTheme;
pub(crate) use self::cache::{zoom_band_for_top};
/// `NOTDEF_RETRY_ATTEMPTS` / `height_epoch` 仅供宿主测试消费
/// （`element/tests/{notdef,prebuild_pass1}.rs`）→ `#[cfg(test)]` 门控，规避生产 lib
/// 构建的 unused import 告警；`notdef_debug` 全仓无消费，随本次清告警一并删除。
#[cfg(test)]
pub(crate) use self::cache::{NOTDEF_RETRY_ATTEMPTS, height_epoch};
pub(crate) use self::cache_host::{CodeParseCache, MathSlotCache};
pub(crate) use self::code_parse::{CodeParseKey, CodeParseLookup, CodeParseState};
pub(crate) use self::prewarm::Prewarm;
pub(crate) use self::theme::{CaretVis, caret_vis, math_color, set_caret_vis};
/// 跨 crate 面（shell/app 预览目录/后台装载装配）：`TocEntries` 目录条目、`WyOpenResult` 后台装载结果。
pub use self::types::{TocEntries, WyOpenResult};
/// 图片编辑卡目标（`open_image_edit` 入参）：宿主 shell 测试端到端打开卡片用。
pub use self::types::ImageEditTarget;
pub(crate) use self::types::{
    BlockLayoutRef, BlockMathPin, BlockRegion, EditKind, ImageEditPopup, ImageResizeDrag,
    ImageSrcBlocks, InlineImageRef, MathEditDrag, MathEditPopup, PrebuildPass1, TocCacheEntry,
    UNDO_LIMIT, UndoUnit, WyDerived, is_plain_region_line, region_line_sig,
};
pub(crate) use self::warm::{warm_emoji_is_done, warm_emoji_rasterizer};

// —— 跨 crate 面收口（shell/app 测试经 `wysiwyg::build_frame` 遍历 `Frame` 内部）——
// `element` 是私有模块，必须把 `Frame` 可达的几何/数据结构在 `wysiwyg::` 层 pub 重导出，
// 才能让 `build_frame` 的返回值跨 crate 可遍历（E0446 要求字段类型 ≥ 容器可见性）。
pub use self::element::{
    BlockCard, BlockLayout, Frame, InlineImageHit, NoteMarker, RowExtra, RowGutter, RowLink,
    ShapedImage, ShapedRow, ShapedTable, TableCellLayout, TableCellsLayout, TableRowCellsLayout,
};
/// 数学渲染子系统（自 `wysiwyg/math` + `math_typeset` 拆为独立 crate `markdown_wysiwyg_math`）：
/// `InlineMathText` 是 `Frame`（pub）携带的字段类型，保持 pub 重导出；其余原
/// `wysiwyg::math::*` 项（`MathSlot`/`inline_math_key`/`splice_math_text`/
/// `display_to_source`/`math_to_*`/`inline_math_fit*` 等）经 `element::*`
///（`element/mod.rs` 的 `pub(crate) use markdown_wysiwyg_math::*`）→ 本文件的
/// `pub(crate) use element::*` glob 链在 `wysiwyg::` 下可达，`crate::wysiwyg::math::X`
/// 语义不变。
pub use markdown_wysiwyg_math::InlineMathText;
pub use markdown_wysiwyg_mermaid::{MmFlowLayout, MmLayout, MmSeqLayout};

// —— mermaid 调色板解耦：`impl From<&WyTheme> for markdown_wysiwyg_mermaid::MmTheme`（11 字段
// 逐一映射）已随 `WyTheme` 迁入引擎 `markdown_wysiwyg_engine::theme`（`WyTheme` 定义在引擎、
// 满足 orphan 规则）；`mm_layout`/`paint_mermaid_layout` 等入口经 `MmTheme::from(&theme)`
// 取用，宿主路径语义不变，markdown_wysiwyg_mermaid 不反向依赖宿主。
pub use crate::ir::IrKind;
pub use crate::markdown::parse::inline_style::MathAlign;
pub use crate::markdown::parse::recognition::LinkHit;


use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{Context, FocusHandle, Focusable, Hsla, Pixels, SharedString, Window, point, px};

use super::doc::Doc;

/// 编辑卡片锚点（图片卡 / 数学卡共用一套 scaffold，收敛 image.rs 与 math_edit.rs 的
/// 两份同构实现）：
/// - 缓存 `Cell<Option<Point>>` + `set`/`clear`/`get`；
/// - [`Self::at`]：paint 期「本帧 `bounds.origin` + 本帧 frame」**同代配对现算**（内容区
///   水平居中 + 顶对齐目标行）——绝不能由 render 直接拿 frame/last_bounds 现算：
///   `end_selection` 清帧后 `ensure_frame_for_input` 用兜底 `px(800)` 宽重建一帧、随后
///   prepaint 又按真实宽重建，render 期每次读到的帧宽不同 → 卡片横向左右跳一帧；
/// - [`Self::resolve`]：**缓存命中读、未命中现算回退**（旧 `match get { Some=>a,
///   None=>现算? }` 逐字同义，回退触发时机不变）。
///
/// 调用方只保留各自的「目标行查找 + 拖拽偏移叠加」差异。
pub(crate) struct CardAnchor {
    anchor: Cell<Option<gpui::Point<Pixels>>>,
    card_w: f32,
}

impl CardAnchor {
    pub(crate) fn new(card_w: f32) -> Self {
        Self {
            anchor: Cell::new(None),
            card_w,
        }
    }

    /// 现算：内容区水平居中 + 顶对齐 `row_top`（帧行坐标 = 文档局部，`origin` 与 `f`
    /// 必须同代）。公式与 image/math 两份旧实现逐字对齐。
    pub(crate) fn at(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
        row_top: f32,
    ) -> gpui::Point<Pixels> {
        let content_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
        let l = f32::from(f.padding) + ((content_w - self.card_w) / 2.).max(0.);
        point(
            px(f32::from(origin.x) + l),
            px(f32::from(origin.y) + row_top),
        )
    }

    /// 回退语义：缓存命中读；未命中 → `compute` 现算并返回（`None` = 无卡片/本帧不画）。
    pub(crate) fn resolve<F>(&self, compute: F) -> Option<gpui::Point<Pixels>>
    where
        F: FnOnce() -> Option<gpui::Point<Pixels>>,
    {
        self.anchor.get().or_else(compute)
    }

    /// 写入缓存（paint 每帧刷新 / open 首帧预填；`None` 清空，同旧 `set`）。
    pub(crate) fn set(&self, a: Option<gpui::Point<Pixels>>) {
        self.anchor.set(a);
    }
    /// 清空缓存（关卡片 / 换源 / 切只读）。
    pub(crate) fn clear(&self) {
        self.anchor.set(None);
    }
    /// 读缓存（拖拽基准 / 卡片包围盒 / 测试访问器）。
    pub(crate) fn get(&self) -> Option<gpui::Point<Pixels>> {
        self.anchor.get()
    }
}

/// 编辑器变更事件（宿主订阅，内容镜像回源码编辑区/保存流程）。
#[derive(Clone, PartialEq, Debug)]
pub enum WyEvent {
    Changed,
    /// 点击内部链接（wiki/tag/block-ref，`[[…]]`/`#tag`/`((id))`）→ 宿主打开对应页面。
    OpenPage(String),
}

/// 「整篇装配」的**纯安装轴**状态（从宿主 `MdWyInstall` 的 `seq` + `pending` + error 收敛
/// 进 WyEditor）：装配代号（[`Self::seq`] 单调递增）/ 在途标志（[`Self::loading`]）/
/// 最近一次失败信息（[`Self::install_error`]）。
///
/// 宿主 async 装配完成回调经 [`WyEditor::begin_install`] / [`WyEditor::finish_install`] /
/// [`WyEditor::fail_install`] 按代号判「是否仍是最新的一代装配」——陈旧（`seq < 当前`）
/// 回调被丢弃，语义 = 原宿主 `md_wy_install.seq == seq` 比对。`MdWyEarly`（early 守卫）与
/// `pending_full`（整篇在途/增量桥收敛标记）这类**收敛标记**仍留宿主，本状态只收纯安装轴。
#[derive(Debug, Default, Clone)]
pub struct WyInstallState {
    /// 后台整篇装配在途：`true` = 已发起异步 `open_from_*` 解析、尚未落地/失败/作废。
    loading: bool,
    /// 最近一次装配失败的宿主可读信息（[`WyEditor::fail_install`] 写入；新装配/成功清除）。
    install_error: Option<String>,
    /// 装配代号（单调计数器）：每次发起整篇装配 / 作废在途递增，结束在途旧装配
    /// （代号不匹配的回调查 [`WyEditor::begin_install`]/[`finish_install`]/[`fail_install`]
    /// 即丢弃，语义 = 原宿主 `md_wy_install.seq`）。
    seq: usize,
}

/// 项目管线的所见即所得编辑器：`Doc`（内部 rope）为唯一数据源，`caret` 是**源字节偏移**。
pub struct WyEditor {
    doc: Doc,
    focus: FocusHandle,
    /// —— 光标闪烁（对齐编辑器 `CodeEditor` / ui 输入框同一模式：聚焦启动、失焦
    /// 退出、epoch 防重、交互静止期常亮；定时器驱动翻转，非每帧重绘）——
    /// 相位状态机收敛自 editor/interaction.rs 与 wysiwyg/core.rs 两份同构实现，
    /// 全部下沉 `editor_core::caret::CaretPhase`（`flip_when_blurred=true`；字段名
    /// `caret_phase` 规避与下方 `caret: usize` 源字节偏移同名的既有字段）。
    caret_phase: editor_core::caret::CaretPhase,
    focus_registered: bool,
    caret: usize,
    /// 是否已装入过完整源（`set_source_text`/`install_open` 置真）。App 大文档 load 走
    /// 后台解析时，`new()` 产生**空**编辑器——在 `install_open` 完成前该编辑器没有内容，
    /// 宿主读取 `text()` 写回必须回退到源码编辑区（否则加载窗口内保存/退出会丢内容）。
    loaded: bool,
    /// 整篇装配的**纯安装轴**（装配代号 seq + 在途 loading + 失败 error）收敛自宿主的
    /// `md_wy_install`（`early`/`pending_full` 等收敛标记仍留宿主）。宿主 async 装配
    /// 回调经 [`Self::begin_install`]/[`Self::finish_install`]/[`Self::fail_install`]
    /// 判陈旧世序 + 记录在途/错误；[`Self::is_loading`] 供宿主「是否整篇装配在途」门控
    /// （替代原宿主 `md_wy_install.pending.is_some()`）。
    install_state: WyInstallState,
    /// 是否可编辑。默认 true（编辑核心用途）；`set_editable(false)` 切**只读预览**门控：
    /// 输入/IME/删除/撤销/粘贴短路（`replace_range` 硬门控）、不画光标；保留光标移动、
    /// 点选/复制、链接点击、折叠、搜索高亮、表格/代码纵向滚动条拖动。
    editable: bool,
    /// EPUB 阅读模式（`set_epub` 打开，EPUB 只读宿主）：标题默认居中（对齐 A 侧预览
    /// `MarkdownPreview::set_epub` 的排版语义）；带 `%%ALIGN:X%%` 指令的行仍以指令为准。
    epub: bool,
    /// **渲染模型状态**单一容器：`WyTheme` / `frame` / `shape_rows` / `prewarm` /
    /// `prebuild_slot` / 布局代数（content_gen/struct_ver/regions_ver…）/ `fold_plan` /
    /// `derived` / `shape_band` / 图量缓存 / 数学与 mermaid 状态 / 横向滚动 6 态 /
    /// `folded_headings` / `marker_hl` / notdef / 数学行数缓存等全部迁入
    /// `markdown_wysiwyg_engine::WyRenderModel`（字段注释见引擎 `model.rs`）。宿主访问
    /// 统一 `self.rd.borrow()/.borrow_mut()`；本字段**之外**的字段均为编辑/宿主语义态
    /// （doc/caret/selection/cache_host/undo/IME/鼠标交互/宿主滚动句柄等，归属表见轮末报告）。
    rd: RefCell<markdown_wysiwyg_engine::WyRenderModel>,
    /// 本帧元素在窗口中的布局 bounds（paint 写入；鼠标命中时全局坐标 → 元素局部）。
    last_bounds: Cell<Option<gpui::Bounds<Pixels>>>,
    /// 字号变化的**缩放锚点** `(块下标, 块内比例)`：`set_font_size` 重排前用
    /// [`Self::viewport_top_px`]（宿主推入的视口顶部）捕捉，帧按新字号重建后由
    /// `advance_zoom_restore`（element prepaint 自驱动）换算出新的滚动目标并写回
    /// `ScrollHandle`——Ctrl+滚轮缩放不带动滚动位置（对齐 `markdown/view`、`editor`
    /// 的视口锚定）。
    zoom_anchor: RefCell<Option<(usize, f32)>>,
    /// 宿主每帧推入的「当前视口顶部 px」（元素局部 y，来自 `#live-scroll` 的
    /// `ScrollHandle::offset` + #live-body 顶部内衬换算）。
    viewport_top_px: Cell<f32>,
    /// 编辑区⇄预览滚动同步的目标块**源码行**（宿主渲染期写入；WY 元素 prepaint 经
    /// [`Self::advance_pending_scroll`] 逐帧重钉宿主偏移直到稳住再清——仿缩放恢复的
    /// 收敛语义，规避「单次写宿主偏移被后续布局复位」）。
    scroll_target: Cell<Option<usize>>,
    /// 上次重钉写进的 `(目标源码行, 目标绝对偏移)`：prepaint 每帧先读宿主当前偏移，
    /// 与它一致才判「已稳住并清除」；否则重写并更新。
    last_pin: Cell<Option<(usize, f32)>>,
    /// 重钉已施加的帧数：每次 `set_scroll_target` 清零，`advance_pending_scroll` 每帧
    /// +1；超 [`Self::SCROLL_TARGET_MAX_APPLIES`] 强制清除（防宿主偏移被滚动条/布局
    /// 持续复位的病态下永不收敛 → 每帧满载重排，仿 `zoom_applies` 的兜底）。
    scroll_applies: Cell<u32>,
    /// 缩放恢复**收敛状态**：`advance_zoom_restore` 逐帧随 `block_top/height` 细化重新计算
    /// 目标（高度估计→整形真值逐帧收敛），直到目标位移 < [`Self::ZOOM_ANCHOR_EPS`] 或
    /// 超过 [`Self::ZOOM_ANCHOR_MAX_APPLIES`] 帧才消费锚点——避免一次性按未收敛高度
    /// 恢复导致视口继续漂移。
    zoom_last_target: Cell<Option<f32>>,
    zoom_applies: Cell<u32>,
    /// 宿主 WYSIWYG 滚动容器句柄 + 内容顶部内衬（`#live-body` py）。注入后
    /// `advance_zoom_restore` 才能在 element prepaint 逐帧把缩放恢复写回 `ScrollHandle`；
    /// 未注入（独立测试）时缩放锚点直接丢弃。
    host_scroll: RefCell<Option<(gpui::ScrollHandle, f32)>>,
    /// 内容/结构/折叠/派生等**渲染模型代数与缓存**（原 WyEditor 字段）已全部迁入
    /// [`Self::rd`]（`WyRenderModel`）：`content_gen`/`shape_band`/`prewarm`/
    /// `block_layout`/`prebuild_slot`/`struct_*`/`regions_*`/`fold_*`/`math_dollar_cnt`/
    /// `alert_marker_cnt`/`pending_patch_blocks`/`derived`/`shape_rows`/`notdef_retry`/
    /// 图量四件套（`image_sizes`/`images_gen`/`image_widths`/`image_src_blocks`/
    /// `pending_image_srcs`）/`math_gen`/`mermaid_zoom(_gen)`/`math_pins`/横向滚动 6 态/
    /// `math_natural_w`/`folded_headings`/`marker_hl(_seq)`/`math_line_map(_warned)`；
    /// 宿主访问点统一 `self.rd.borrow()/.borrow_mut()`（失效语义逐字保持）。
    /// 图片异步加载缓存（gpui `ImageAssetLoader` 实体；命中返回 `Arc<RenderImage>`）。
    image_cache: gpui::Entity<gpui::RetainAllImageCache>,
    /// 换源（`install_open`）置脏 → 下一帧 [`Self::render`] 的 `flush_image_cache_drop`
    /// 统一 `clear` 释放旧文档图片纹理。`install_open` 无 cx/window，释放延迟到每帧；
    /// gpui `RetainAllImageCache` 是 `HashMap` **无淘汰**，漏清 → 同一 WyEditor 反复
    /// 打开不同含图文档时 GPU 纹理只涨不降（内存 bug 根因）。
    /// `sync_math`/`pending_math_drop` 同款「脏标记 + 每帧统一 flush」姿势（K10）。
    image_cache_dirty: Cell<bool>,
    /// 进行中的图片调整条拖拽（右中边缘）。`Some` = 按住拖动中（随鼠标改宽，不改源码）。
    image_resize_drag: RefCell<Option<ImageResizeDrag>>,
    /// LaTeX → 排版结果缓存（参考 zorite `<MathStore>` 按公式源码做键）。
    /// `Loading` 排版中 / `Ready` 就绪（位图 + 逻辑 px 尺寸）/ `Failed` 失败（回退 Unicode）。
    /// 容量上限 + LRU（见 [`MathSlotCache`]）：无界 HashMap 会让同一标签页反复换公式
    /// 文档时位图/GPU 纹理只涨不降。
    math_slots: Rc<RefCell<MathSlotCache>>,
    /// 排版文字色；主题切换 → 清缓存重排（同 zorite `MathStore::set_color`）。
    math_color: RefCell<Option<Hsla>>,
    /// 当前获得焦点的 mermaid 卡（`src_line`）：**点击卡片**置位、点击其它处清除。
    /// 仅聚焦的 mermaid 用滚轮缩放（`on_scroll_wheel` 只消费聚焦卡上的滚轮），未聚焦时
    /// 滚轮一律放行给宿主正常滚动——避免「往下滚遇到 mermaid 就缩图」。视图本地态，不进撤销栈。
    focused_mermaid: Option<usize>,
    /// 代码块高亮解析缓存（后台 syntect 整块解析；`request_code_parse` 置 Pending、
    /// `sync_code_parse` 后台跑 `parse_code_lines` 后落 Done）。解析就绪由
    /// `on_code_parse_ready` **定向作废该代码块的行整形缓存条目**（非全局代数）→ 下帧
    /// 重建用高亮行替换占位纯文本行。
    code_parse: std::sync::Arc<std::sync::Mutex<CodeParseCache>>,
    /// 本帧登记的待后台解析队列 `((语言, 代码), 块 src_line)`（`sync_code_parse` 每帧
    /// 清空消费）。`src_line` 用于完成时的**带内门控**（块还在带内才立即重建帧上色）。
    code_parse_need: RefCell<Vec<(CodeParseKey, usize)>>,
    /// 代码块高亮请求的「块内容指纹」钉（`request_code_parse` 时 `src_line →
    /// `frame::block_content_id` 快照）。`on_code_parse_ready` 完成回调按此钉作废整形
    /// 缓存、替代「当刻 `src_line → block`」反查——增量编辑行平移后旧 src_line 反查会
    /// 作废同一位置的无关块、漏作废真正的内容块（P1-7 编辑错位）；内容指纹不随块
    /// 下标/行号位移，`src_line` 值经请求队列传到完成回调时仍可反查回对的快照。
    code_parse_pins: RefCell<std::collections::HashMap<usize, u64>>,
    /// 换源（`install_open`）清 `math_slots` 时收拢的旧文档 Ready 位图纹理：
    /// `install_open` 无 `cx`，纹理释放推迟到下一帧 `sync_math` 用 `cx.drop_image`
    /// 统一释放（Arc 未 drop 前字节仍在、可重传，不丢渲染）。有界：每次换源一批、
    /// 每帧清空。
    pending_math_drop: RefCell<Vec<std::sync::Arc<gpui::RenderImage>>>,
    /// 数学排版任务的「换源代」：`install_open` 换源自增。在途后台光栅完成时以此判定
    /// 任务是否跨了换源——跨源旧任务不写新文档槽、不触发就绪（新文档会自行 claim）。
    math_epoch: Cell<u64>,
    /// 代码高亮解析任务的「换源代」：同 [`Self::math_epoch`]——旧文档解析完成后不写
    /// 新文档缓存、不按旧 `src_line` 作废新文档的块。
    code_epoch: Cell<u64>,
    /// 行内公式键（`"inline:…"`）→ 包含它的 `(行 src_line, 块内容指纹)` 对（`sync_math`
    /// claim 时登记）。`on_math_ready` 完成时按**注册时捕获的块内容指纹**定向作废这些行
    /// 整形缓存条目——替代 mgen 进全局 shape_key 的整带 miss（`$$` 显示块在 Pass2 被
    /// blocked、不入 `shape_rows`，无需作废）；指纹随行平移稳定，避免完成回调按当刻
    /// `src_line → block` 反查在增量编辑行平移后作废错块（P1-7）。
    /// 换源清空（行号随源失效）。
    inline_math_rows: RefCell<std::collections::HashMap<SharedString, Vec<(usize, u64)>>>,
    /// 图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    base_dir: Option<std::path::PathBuf>,
    /// 内存图片注册表：虚拟 src（`epubimg://…`）→ 图片字节（EPUB 不落盘）。
    /// 渲染时 paint 直接查表解码（`set_memory_images` 注入，`memory_image` 读取）。
    memory_images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    /// IME 组合（marked text）的源字节区间。空 = 无组合进行中。
    /// 关键：组合的每次更新都**替换**这段（而非追加到后面）——不追踪它就会把
    /// 上一次组合的字符重复插入（输入“你好”→ “nnini'hni'hao你好”的坏字符）。
    marked: Option<Range<usize>>,
    /// 文本选区的固定锚点（源字节）。`Some(anchor)` 时选区 = `anchor..caret`（caret
    /// 为活动端）；`None` = 无选区（仅光标）。拖拽/ Shift+方向 产生，Ctrl+C/A/V 消费。
    sel_anchor: Option<usize>,
    /// A4：目录缓存 `(content_gen, 条目)`——内容变更才重扫 Doc 块（侧边栏目录每帧
    /// 渲染消费，`toc_entries` 按此 O(1) 命中，避免大文档每帧 O(块数) 扫描）。
    toc_cache: RefCell<Option<TocCacheEntry>>,
    /// 鼠标左键拖选进行中（on_mouse_move 延伸选区）。
    dragging: bool,
    /// 预览独有能力「搜索跳转高亮」（自 [`MarkdownPreview::highlight_search`] 搬入）：
    /// （源码行, 小写查询词）；`Some` 时 paint 只给该行中命中词的源区间画背景色，
    /// [`config::timing::HIGHLIGHT_MS`] 后自动消失（`highlight_seq` 防旧定时器误清）。
    highlight_search: Option<(usize, String)>,
    /// 高亮代序列号：每次发起/清除高亮 +1，定时器只清除同代高亮。
    highlight_seq: u64,
    /// 代码/表格/数学横向滚动 6 态（`code_h_scroll`/`code_h_drag`/`table_h_scroll`/
    /// `table_h_drag`/`math_h_scroll`/`math_h_drag`）已随 P3 / Slice C 收敛为
    /// [`markdown_wysiwyg_engine::model::HScrollState`]（`h_scroll` 字段，按 `HScrollKind`
    /// 分桶）；数学卡自然宽侧缓存（`math_natural_w`）、已折叠标题区块（`folded_headings`）
    /// 仍为 `WyRenderModel` 直接字段（见引擎 `model.rs` 对应字段注释）。宿主访问
    /// `self.rd.borrow().h_scroll.set_scroll(...)` 等（失效语义不变）。
    /// 当前文档中每个可编辑块的**区域**（代码内容行/表格表头+数据行）与**围栏行**
    /// （代码 ``` 标记行、表格分隔行，只读）。导航/退格/删除据此避免光标落进
    /// 只读围栏（否则在隐藏围栏上输入会破坏结构）。随文本变更重建。
    block_regions: Vec<BlockRegion>,
    /// 撤销栈（pop → 应用于当前内容）；重做栈。**精确逆操作**（净一次编辑 / 单元），
    /// 深度 [`UNDO_LIMIT`]；**B2 合并**：连续同意图且邻接的编辑并入当前打开的
    /// 单元（`open_unit`），撤销把净逆操作经增量管道打回——不再整体重建模型。
    undo_stack: Vec<UndoUnit>,
    redo_stack: Vec<UndoUnit>,
    /// 正在累积的撤销单元（最后一批并入它的编辑尚未固化；下次非合并编辑或 undo/redo
    /// 时关闭并入栈）。
    open_unit: Option<UndoUnit>,
    /// 上一个编辑（意图 + 源字节区间）：`replace_range` 用它判定是否与当前单元合并成
    /// 同一撤销事务（连续打字 / 相邻退格 / 同点删除）；光标移动（`set_caret`/方向键
    /// `moved`）与整篇内容替换打断。
    last_edit: Option<(EditKind, usize, usize)>,
    /// 上一个编辑发生的时刻（I1 时间窗）：与它停顿 ≥`UNDO_GROUP_MS`（300ms）即新起
    /// 撤销单元（持续打字合并、停顿打断，对齐 `editor/undo.rs`）。与 `last_edit` 同生共灭。
    last_edit_at: Option<std::time::Instant>,
    /// WYSIWYG「编辑日志」（方案2：切回编辑区按序重放，取代行级 diff 的主路径）。
    /// `Some` = 正在收集（仅 live2 实例被 App 开启，`md_wy` 右栏预览等非 live 编辑不
    /// 收集）；`None` = 关闭。收集内容 = `replace_range` / undo / redo 统一缴存的
    /// `(字节区间, 新文本)` op——两者从同一「toggle ON 的源码 Rope 快照」起步、live 会话
    /// 期间源码编辑区不被改写 → op 字节偏移在源码编辑区与 live2 坐标系恒等，按序重放
    /// 即得最终态（整篇换源 `install_open` 时清空，防后台加载窗口内的陈旧 op 错位）。
    /// 提交（App `commit_live`）取空后实体即销毁。
    live_ops: Option<Vec<(Range<usize>, SharedString)>>,
    /// 当前悬浮的图片行 `src_line`（`on_mouse_move` 更新）。非 `None` 时该图片右上角
    /// 浮现「编辑」铅笔按钮（仅可编辑模式绘制/响应）。纯视图态，不进撤销栈。
    hover_image: Cell<Option<usize>>,
    /// 点击图片**本体**时置位的选中图片行 `src_line`。非 `None` 时该图片画 accent
    /// 描边 + 编辑按钮常显（不依赖指针悬浮）。与 `hover_image` 不同：选中态独立于
    /// 指针，点击其它文本区 / 换源 / 只读 时清除；Backspace/Delete 删除整张图片行。
    /// 纯视图态，不进撤销栈。
    selected_image: Cell<Option<usize>>,
    /// 当前悬浮的**行内**图片（`InlineImageRef`；`on_mouse_move` 更新）。非 `None` 时该
    /// 行内图右上角浮现编辑按钮（仅可编辑模式绘制/响应）。纯视图态，不进撤销栈。
    hover_inline_image: Cell<Option<InlineImageRef>>,
    /// 点击行内图片**本体**时置位的选中行内图（`InlineImageRef`）。非 `None` 时该行内图
    /// 画 accent 描边 + 编辑按钮常显（不依赖指针悬浮）。与 `hover_inline_image` 不同：
    /// 选中态独立于指针，点击其它文本区 / 换源 / 只读 时清除；Backspace/Delete 删除
    /// 整段 `![…](…)`。纯视图态，不进撤销栈。
    selected_inline_image: Cell<Option<InlineImageRef>>,
    /// 图片编辑卡片（打开时在图片旁渲染悬浮表单：链接 + 文本两个输入框）。
    /// `None` = 未打开。打开期间输入框获得焦点（IME 走 `InputState`，与文档输入互不干扰）。
    image_edit: Option<ImageEditPopup>,
    /// 图片编辑卡片的**窗口锚点**缓存（卡片 TopLeft，window 坐标）：paint 期「本帧
    /// `bounds.origin` + 本帧 frame」同代配对现算缓存，render 只读 + 未命中回退现算；
    /// 绝不能由 render 直接拿 frame/last_bounds 现算——`end_selection` 清帧后
    /// `ensure_frame_for_input` 会用兜底 `px(800)` 宽重建一帧、随后 prepaint 又按真实宽
    /// 重建，render 期每次读到的帧宽不同 → 卡片横向左右跳一帧（「突然往左闪烁」）。
    /// `None` = 无卡片 / 锚点未就绪。实现见 [`CardAnchor`]，行查找差异在 image.rs。
    image_edit_anchor: CardAnchor,
    /// 块级数学公式（`$$…$$`）的编辑卡片（双击卡弹出，预填正文多行输入区域）。
    /// `None` = 未打开。打开期间输入区域获得焦点（内嵌 CodeEditor，与文档输入互不
    /// 干扰）；每键经订阅即时写回文档（`apply_edit` 撤销栈、实时重渲染），点卡片外
    /// 任意处关闭（无取消语义）。仅可编辑模式打开；只读模式双击 = no-op（保持渲染卡）。
    math_edit: Option<MathEditPopup>,
    /// 数学编辑卡片的**窗口锚点**缓存（卡片 TopLeft，window 坐标）：paint 期同代配对
    /// 现算缓存，render 只读 + 未命中回退现算（同 [`CardAnchor`] 纪律并与图片编辑卡
    /// 共用一套实现；`math_edit_delta` 拖拽偏移叠加在 math_edit.rs 的现算内）。
    math_edit_anchor: CardAnchor,
    /// 数学编辑卡片输入区域（CodeEditor）的 `Edited` 订阅（每键写回文档的唯一入口）。
    /// 随打开重建、随关闭 / 换源 / 切只读 drop（旧输入实体随之失效）。
    math_edit_sub: Option<gpui::Subscription>,
    /// 数学编辑卡片**拖拽把手**进行中（标题栏按下置位；`on_mouse_up` / 窗口级 mouse up
    /// / 新按下 / 换源 / 切只读清除）。有它 = 本帧正在拖动卡片。
    math_edit_drag: Cell<Option<MathEditDrag>>,
    /// 数学编辑卡片相对文档锚点的**累计偏移**（px，默认 0）：标题栏拖拽累计
    /// `pre_delta + (pos − grab)`；paint 期刷新锚点 = `anchor_at(origin,f) + delta`
    /// （否则每帧 paint 把拖拽偏移冲掉）；open/close 重置 0。
    math_edit_delta: Cell<gpui::Point<Pixels>>,
    /// EPUB 注记正文表（`注记号 → 注释文本`，`set_notes` 注入；悬浮卡正文优先读它）。
    note_ctx: std::collections::BTreeMap<usize, SharedString>,
    /// 当前悬浮的注记标记（命中矩形 = 内容列局部，卡锚定用 `content_origin` 换算；
    /// 由 `on_mouse_move`/窗口级 move 更新，注视离开/换源/点击跳转后清除）。
    hover_note: Cell<Option<element::NoteMarkerHit>>,
    /// 引文跳转高亮（`marker_hl`/`marker_hl_seq`：源码行与代序列号）已迁入 [`Self::rd`]
    ///（`WyRenderModel`）；paint 消费 `self.rd.borrow().marker_hl`，宿主导航写
    /// `self.rd.borrow_mut().marker_hl.set(…)`。
    /// 注记标记 `[n]` 按下但**尚未确认是单击**的命中（P4-2 回归修复）：按下不立即跳转，
    /// 等 [`NOTE_NAV_CONFIRM_MS`] 的单击确认窗口；期间出现第二击（双击）即取消——
    /// 否则双击注记段落首击就整页位移 + 加亮闪烁（用户报告「某些段落双击 → 闪烁变动/
    /// 文字掉到其他行」，EPUB 尾部 90 条注记尤其严重）。`Some` 且窗口无双击 → `navigate_marker`。
    note_down: Cell<Option<element::NoteMarkerHit>>,
    /// 单击确认代的序列号：每次按下/取消 +1，旧窗口的定时器对不上代即静默（防多次
    /// 双击后残留的定时器误触发跳转）。
    note_down_seq: Cell<u64>,
    /// 引文跳转索引缓存：`(content_gen, Rc<CitationIndex>)`——懒建（首次交互到注记才
    /// O(块数) 建一次），内容变更后自动失效重建。「行」字段语义 = **源码行**。
    cit: RefCell<Option<(u64, std::rc::Rc<crate::markdown::parse::CitationIndex>)>>,
}
/* ---------- 键盘 / IME 输入 ---------- */

/// 键上下文（Render 的 div 挂载；键位由 [`bind_keys`] 装配）。
pub const CONTEXT: &str = "wy-editor";

/// Math 卡专用键上下文（内嵌 CodeEditor 的承载体 div 挂载）：内层 context 深度优先，
/// 把外层 `CONTEXT`（wy-editor）的 `enter → wysiwyg::Enter` 从 match_result 压制掉 →
/// 动作层放空 → key_down 照常派发进 CodeEditor 原样换行（卡片内聚焦须经本 context，
/// 否则 Enter 仍漏到 WY 根向文档插换行）。
pub const MATH_EDIT_CONTEXT: &str = "wy-math-edit";

macro_rules! wy_action {
    ($name:ident, $s:literal) => {
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name;
        impl gpui::Action for $name {
            fn boxed_clone(&self) -> Box<dyn gpui::Action> {
                Box::new(self.clone())
            }
            fn partial_eq(&self, other: &dyn gpui::Action) -> bool {
                (other as &dyn std::any::Any)
                    .downcast_ref::<Self>()
                    .is_some()
            }
            fn name(&self) -> &'static str {
                $s
            }
            fn name_for_type() -> &'static str {
                $s
            }
            fn build(_: serde_json::Value) -> anyhow::Result<Box<dyn gpui::Action>> {
                Ok(Box::new(Self))
            }
        }
    };
}
wy_action!(Left, "wy-left");
wy_action!(Right, "wy-right");
wy_action!(Up, "wy-up");
wy_action!(Down, "wy-down");
wy_action!(SelectLeft, "wy-select-left");
wy_action!(SelectRight, "wy-select-right");
wy_action!(SelectUp, "wy-select-up");
wy_action!(SelectDown, "wy-select-down");
wy_action!(Backspace, "wy-backspace");
wy_action!(Delete, "wy-delete");
wy_action!(Enter, "wy-enter");
wy_action!(SelectAll, "wy-select-all");
wy_action!(Copy, "wy-copy");
wy_action!(Cut, "wy-cut");
wy_action!(Paste, "wy-paste");
wy_action!(Undo, "wy-undo");
wy_action!(Redo, "wy-redo");
// E3：Tab 键进 WY。必须在 `wy-editor` 上下文注册键位（深于 Root 的 `tab→focus_next`），
// 否则 WY 持焦时 Tab 被窗口级焦点循环劫走（用户「按 Tab 视觉无任何效果」的根因）。
wy_action!(Tab, "wy-tab");

/// 装配键位（入口：`main.rs` 与其它键位一并安装）。
pub fn bind_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-z", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        // E3：Tab → 光标处插入可见缩进（`\t` 字面量，与源码编辑区同一单位）。
        KeyBinding::new("tab", Tab, Some(CONTEXT)),
        // Math 卡（内嵌 CodeEditor）内层上下文：enter→NoAction 压制外层 CONTEXT 的
        // enter→wysiwyg::Enter。否则动作层先命中该绑定并 stop（gpui 先动作层后
        // key_down）→ CodeEditor 的 key_down 换行永不执行、且换行错误地插到文档。
        // NoAction 在 match_result 里放空 → key_down 照常派发进 CodeEditor 原样换行。
        KeyBinding::new("enter", gpui::NoAction, Some(MATH_EDIT_CONTEXT)),
    ])
}

impl Focusable for WyEditor {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl gpui::EventEmitter<WyEvent> for WyEditor {}

impl gpui::Render for WyEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{CursorStyle, InteractiveElement, ParentElement, Styled};
        // 换源脏标记消费：本帧（宿主重绘入口）统一 `clear` 释放旧文档图片纹理
        //（`install_open` 置脏、无 cx/window，延迟到每帧 render 这里；见
        // `flush_image_cache_drop`。与 `sync_math` 的 `pending_math_drop` 同款每帧 flush）。
        self.flush_image_cache_drop(window, cx);
        // 图片编辑卡片浮层：在元素树根上**最后**叠加（最后绘制、命中最靠前），
        // 锚定在目标图片旁渲染（打开时才有）。卡片几何取自 paint 期缓存的
        // `image_edit_anchor`（同代 origin+frame），不在此现算帧（避免读到 ensure
        // 兜底 800 宽重建的临时帧而横跳）。
        let image_edit_overlay = self.image_edit_card_element(window, cx);
        // 数学编辑卡片浮层：同样最后叠加、锚定在目标公式卡顶部（打开时才有）。
        let math_edit_overlay = self.math_edit_card_element(window, cx);
        // P4-2: 注记悬浮卡片浮层（只读宿主悬停注记标记时）：同样最后叠加、锚定在注记
        // 矩形右下方（窗口坐标，paint 期 `hover_note` 的内容局部矩形 + `content_origin`）。
        let note_overlay = self.note_card_element(window, cx);
        let mut root = gpui::div()
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Left, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_left()
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Right, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_right()
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Up, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_up()
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Down, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_down()
                },
            ))
            // S1：Shift+方向键 → 与编辑区同语义的建立/收缩选区（固定锚 sel_anchor）。
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &SelectLeft, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_left_select(true)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &SelectRight, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_right_select(true)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &SelectUp, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_up_select(true)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &SelectDown, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.move_down_select(true)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Backspace, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.backspace(cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Delete, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.delete(cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Enter, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.newline(cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &SelectAll, _: &mut Window, _: &mut Context<WyEditor>| {
                    this.select_all()
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Copy, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.copy(window, cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Cut, window: &mut Window, cx: &mut Context<WyEditor>| {
                    this.cut(window, cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor,
                 _: &Paste,
                 window: &mut Window,
                 cx: &mut Context<WyEditor>| { this.paste(window, cx) },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Undo, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.undo(cx)
                },
            ))
            .on_action(cx.listener(
                |this: &mut WyEditor, _: &Redo, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.redo(cx)
                },
            ))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(
                    |this: &mut WyEditor,
                     ev: &gpui::MouseDownEvent,
                     window: &mut Window,
                     cx: &mut Context<WyEditor>| {
                        this.on_mouse_down(ev, window, cx)
                    },
                ),
            )
            .on_mouse_move(cx.listener(
                |this: &mut WyEditor,
                 ev: &gpui::MouseMoveEvent,
                 _: &mut Window,
                 cx: &mut Context<WyEditor>| { this.on_mouse_move(ev, cx) },
            ))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(
                    |this: &mut WyEditor,
                     ev: &gpui::MouseUpEvent,
                     _: &mut Window,
                     cx: &mut Context<WyEditor>| { this.on_mouse_up(ev, cx) },
                ),
            )
            // 鼠标**在元素外松开**（Capture 阶段、`!is_hovered`）：拖动到元素外松手时，
            // 上面的 `on_mouse_up` 受 hitbox 门控不会派发——这里与 `on_window_mouse_up`
            // 同语义清掉图片调整条拖拽（idempotent：未拖拽时无副作用）。这是「松开后
            // 纯移动不改宽」四条边界之一（对齐本仓 scrollbar/video/image 的
            // `on_mouse_up_out` 拖拽清态惯例）。
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(
                    |this: &mut WyEditor,
                     _ev: &gpui::MouseUpEvent,
                     _: &mut Window,
                     cx: &mut Context<WyEditor>| { this.on_window_mouse_up(cx) },
                ),
            )
            .child(element::WyEditorElement {
                editor: cx.entity(),
            });
        // E3（只读门控）：Tab 动作只在**可编辑**时登记。只读预览（md_wy/epub）中该
        // `on_action` 不注册 → `wy-tab` 键位虽仍在 `wy-editor` 上下文内解析、却无监听可
        // 消费 → 按键落回 Root 的 `tab→focus_next`（焦点循环不回归；只读不可编辑、插
        // Tab 无意义）。gpui 动作冒泡默认停传（window.rs bubble 置 propagate=false），
        // 可编辑时 `tab_insert` 消费即拦住 Root 焦点循环——两态分路互不干扰。
        if self.editable {
            root = root.on_action(cx.listener(
                |this: &mut WyEditor, _: &Tab, _: &mut Window, cx: &mut Context<WyEditor>| {
                    this.tab_insert(cx)
                },
            ));
        }
        if let Some(overlay) = image_edit_overlay {
            root = root.child(overlay);
        }
        if let Some(overlay) = math_edit_overlay {
            root = root.child(overlay);
        }
        if let Some(overlay) = note_overlay {
            root = root.child(overlay);
        }
        root
    }
}
/// （测试探针）跨 crate 测试访问器：theme 单一来源断言用（原 `#[cfg(test)]` impl 转无条件）。
impl WyEditor {
    pub fn test_theme_fg(&self) -> gpui::Hsla {
        self.rd.borrow().theme.text
    }
    pub fn test_theme_code_bg(&self) -> gpui::Hsla {
        self.rd.borrow().theme.code_bg
    }
    pub fn test_theme_link(&self) -> gpui::Hsla {
        self.rd.borrow().theme.link
    }
    pub fn test_theme_is_dark(&self) -> bool {
        self.rd.borrow().theme.is_dark
    }
    /// （仅测试）当前缴存中的编辑日志 op 条数（未开启收集 = 0）。app 回归用实例级
    /// 确定性断言「单字符恰 1 条 / 撤重混用恰 N 条 / 大粘贴单大 op」，不依赖进程级静态。
    pub fn live_ops_len(&self) -> usize {
        self.live_ops.as_ref().map_or(0, |v| v.len())
    }
}
