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

mod cache;
mod code_parse;
mod content_sync;
mod core;
mod edit;
mod image;
mod mouse;
mod prewarm;
mod theme;
mod types;
mod view;
mod warm;

mod element;
pub(crate) mod frame;
mod input;
pub(crate) mod math;
mod math_typeset;
pub(crate) mod mermaid;
pub(crate) mod shape;

// 子模块扁平重导出：`element::*` 把布局类型 + geom/math/shape 的 pub(crate) 项一并带进
// mod.rs 命名空间，frame/shape/math 以 `use super::*` 共享（对应 `element::*` 路径原样保留）。
// frame 不整块重导出（其内部 `warm_emoji_rasterizer` 与根级同名函数冲突），由 element.rs
// 经 `pub(crate) use super::frame::{…}` 精确暴露。
pub(crate) use element::*;

// —— M2.1 拆出的子模块项按原可见性扁平重导出（外部/兄弟对 `wysiwyg::…` 的引用不变）——
pub use self::theme::WyTheme;
pub(crate) use self::cache::{
    NOTDEF_RETRY_ATTEMPTS, NotdefRetry, RowShapeCache, height_epoch, notdef_debug,
    zoom_band_for_top,
};
pub(crate) use self::code_parse::{CodeParseKey, CodeParseLookup, CodeParseState};
pub(crate) use self::prewarm::Prewarm;
pub(crate) use self::theme::{CaretVis, caret_vis, math_color, set_caret_vis};
pub(crate) use self::types::{
    BlockLayoutRef, BlockMathPin, BlockRegion, EditKind, ImageEditPopup, ImageResizeDrag,
    ImageSrcBlocks, MathSlot, PrebuildBlock, PrebuildPass1, TocCacheEntry, TocEntries,
    UNDO_GROUP_MS, UNDO_LIMIT, UndoUnit, WyDerived, WyOpenResult, inline_math_key,
    is_plain_region_line, region_line_sig,
};
pub(crate) use self::warm::{warm_emoji_is_done, warm_emoji_rasterizer};

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{Context, FocusHandle, Focusable, Hsla, Pixels, SharedString, Window, point, px};

use super::doc::Doc;

/// 编辑器变更事件（宿主订阅，内容镜像回源码编辑区/保存流程）。
#[derive(Clone, PartialEq, Debug)]
pub enum WyEvent {
    Changed,
    /// 点击内部链接（wiki/tag/block-ref，`[[…]]`/`#tag`/`((id))`）→ 宿主打开对应页面。
    OpenPage(String),
}
/// 项目管线的所见即所得编辑器：`Doc`（内部 rope）为唯一数据源，`caret` 是**源字节偏移**。
pub struct WyEditor {
    doc: Doc,
    focus: FocusHandle,
    caret: usize,
    /// 是否已装入过完整源（`set_source_text`/`install_open` 置真）。App 大文档 load 走
    /// 后台解析时，`new()` 产生**空**编辑器——在 `install_open` 完成前该编辑器没有内容，
    /// 宿主读取 `text()` 写回必须回退到源码编辑区（否则加载窗口内保存/退出会丢内容）。
    loaded: bool,
    /// 是否可编辑。默认 true（编辑核心用途）；`set_editable(false)` 切**只读预览**门控：
    /// 输入/IME/删除/撤销/粘贴短路（`replace_range` 硬门控）、不画光标；保留光标移动、
    /// 点选/复制、链接点击、折叠、搜索高亮、表格/代码纵向滚动条拖动。
    editable: bool,
    /// EPUB 阅读模式（`set_epub` 打开，EPUB 只读宿主）：标题默认居中（对齐 A 侧预览
    /// `MarkdownPreview::set_epub` 的排版语义）；带 `%%ALIGN:X%%` 指令的行仍以指令为准。
    epub: bool,
    theme: WyTheme,
    /// 本帧行网格（measure→prepaint→paint 复用；**窗口化**：仅带内块被真正 shape，
    /// 带外块用 `BlockMeta` 估算高度占位，避免大文档整篇每帧 shape 卡死）。
    frame: RefCell<Option<std::rc::Rc<element::Frame>>>,
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
    /// 内容代数：任何文本变更自增；`request_layout` 用它判断帧缓存是否仍有效。
    content_gen: Cell<u64>,
    /// 可视带（窗口坐标→元素局部 y，量程化）：prepaint 用 `bounds.origin.y` +
    /// `viewport_size` 推算并发布，measure 用上一帧的带做窗口化 shape。
    shape_band: Cell<Option<(f32, f32)>>,
    /// **滚动预热**状态（`element::prewarm_band`）：把下一量子带块提前 shape 进
    /// `shape_rows` 缓存（键与 band 无关）→ 带穿越的 6.4ms/band shape 突发被摊平。
    /// 方向由带序号跨界判定；结构编辑清空整形缓存后必须归零（见 `replace_range`）。
    prewarm: RefCell<Prewarm>,
    /// 全篇**基础布局**缓存：`(代, Rc<RefCell<BlockLayout>>)`——rolling/带移动与
    /// **非结构性编辑**都复用（只对受影响块做 Fenwick 点更新），代 ≠ 内容代：
    /// 由 `struct_ver`/宽度/mermaid 缩放组成（**不含 `images_gen`/`mgen`**——图片/数学高
    /// 变更走稀疏定向覆写 / `fold_plan` 稀疏 delta，见 `height_epoch` eval）。结构性变更
    /// （换行/围栏/表格等改了块数或块序）才整篇重建。`set_theme`/`set_font_size` 把代置
    /// 失效位强制重算。
    block_layout: RefCell<Option<(u64, Rc<std::cell::RefCell<element::BlockLayout>>)>>,
    /// 首帧基础布局**预建输入槽**（Step4-①）：`install_open` 存 `WyOpenResult.prebuild`，
    /// 首个 `build_frame`（`rebuilt` 分支）经 [`frame::pass1_from_prebuild`] **一次性**
    /// 用真实主题/宽度物化成 `BlockLayout` 后清空——首帧不再全量 doc 估高。任何编辑
    /// （`struct_ver > 0`）/mermaid 缩放（`zgen != 0`）/块数不齐 → 守卫放行走既有
    /// `pass1_base_layout` 兜底（正确优先）。
    prebuild_slot: RefCell<Option<PrebuildPass1>>,
    /// 内容**结构版本**：`replace_range` 里结构性变更（跨行、代码/表格边界）才递增——
    /// 参与基础布局代；非结构性编辑不递增（基础布局跨键复用，只 patch 受影响块高）。
    struct_ver: Cell<u64>,
    /// 结构编辑「行号平移」版本（M5-B3-1 子项 2/4）：`shift_derived_after_edit` 成功
    /// 就地平移区域行号（结构不变、只是行号动）时递增——纳入 [`Self::fp_sets`] 缓存键，
    /// 防集合命中旧行号；`regions_ver++` 整篇重建走另一条键分量（`regions_ver`）天然
    /// 失效。
    regions_shift_ver: Cell<u64>,
    /// 结构性编辑影响的**首个源行**（Pass1 增量高度重估的起点）：编辑线之前的块
    /// 内容未变 → 复用旧布局高度；只有「编辑线起 → 文尾」重新估算（Enter 贴近文尾时
    /// O(变化后缀) 而非全篇）。非结构编辑不参与（无重建）。
    struct_dirty_line: Cell<usize>,
    /// 编辑影响的**最后一个旧源行**（`edit_line + 被删换行数`）：Pass1 用它定位旧尾部
    /// 起点（`j`），从而**复用尾部高度**（编辑下方块内容未变，高度不变）——把结构编辑
    /// 从「dirty_line→文尾」全后缀重估降为「仅变化中段」+ 尾部 Vec 复制。
    struct_old_end_line: Cell<usize>,
    /// 编辑的源行净增（插入换行数 − 被删换行数）：尾部块 src_line 整体平移量。
    struct_line_delta: Cell<i64>,
    /// 结构性编辑被编辑块的**旧块下标**（编辑前）与**块数净增**（编辑后 − 编辑前）：
    /// `fold_plan` deltas 增量平移用（块下标 ≥ 旧块下标之后整体 + 块数净增）。
    struct_blk_old: Cell<usize>,
    struct_blk_span_delta: Cell<i64>,
    /// 结构性编辑待消费标志：结构性分支置位、`fold_plan` 消费后清除——防止增量平移
    /// 误用陈旧编辑信息（非结构编辑/宽度变化恰在结构编辑之后、其间无重建时）。
    struct_edit_pending: Cell<bool>,
    /// 内容**派生版本**——`derived()` 缓存键：结构性编辑、编辑行 region 签名变化才
    /// 递增；纯文本键入（非结构、签名不变、不在数学块内）不递增 → `derived` Rc
    /// 跨键复用，不再每键对全篇 `math_regions`/`alert_*` 重扫（7MB/结构密集文档
    /// 「输入卡死」的主因，旧实现每键 2 次全篇扫描 ≈1s）。
    regions_ver: Cell<u64>,
    /// 折叠/重排计划缓存：`(key, marker_key, Rc<FoldPlan>)`——与可视带无关（内容、caret
    /// reveal、数学就绪、宽度、结构版本决定），滚动/带移动复用；build_frame 每档不再
    /// 重建 ~万行 Hash 集合 + 逐块二分（大文档滚动「卡顿明显」：~40ms/档）。
    /// `marker_key` = regions_ver^mgen^fold_ver：marker 身份在结构编辑间未变 → deltas
    /// 可增量平移复用（`fold_plan_incremental`）。
    fold_plan: RefCell<Option<(u64, u64, Rc<element::FoldPlan>)>>,
    /// 折叠计划 **marker 集合**缓存：`(sets_key, Rc<FoldPlanSets>)`。`sets_key` =
    /// `regions_ver ^ regions_shift_ver ^ fold_ver`（只 region/折叠结构，**不含布局
    /// epoch / mgen**）——布局换代、宽度/字号、公式光栅就绪（mgen）只改 `deltas`（高度），
    /// marker 集合跨计划复用（M5-B3-1 子项 2，免每次 O(#区域) 哈希重建）。
    fp_sets: RefCell<Option<(u64, Rc<element::FoldPlanSets>)>>,
    /// 标题折叠视图态版本：`toggle_heading_fold_at_line` 翻转 `folded_headings` 时递增
    /// （纳入 `fold_plan` 缓存键，滚动不带折叠态重算）。
    fold_ver: Cell<u64>,
    /// `$` 出现次数（内容级增量计数；`derived` 的数学预筛用，免全篇重扫）。
    math_dollar_cnt: Cell<usize>,
    /// `[!` 出现次数（`derived` 的警报/callout 预筛用，免全篇重扫）。
    alert_marker_cnt: Cell<usize>,
    /// 最近一次编辑受影响（需重估高度）的块下标；`build_frame` 消费后清空。
    /// `None` = 无（或结构性编辑 → 整篇重建，不进此表）。
    pending_patch_blocks: RefCell<Option<Vec<usize>>>,
    /// 内容派生扫描缓存：`(regions_ver, Rc<RefCell<WyDerived>>)`——滚动/带移动与纯文本
    /// 键入复用；结构性/签名变化重建。`RefCell` 供编辑期对数学块 `source` 做局部修补
    /// （`replace_range`，避免键入大文档数学块内部也整篇重建）。
    derived: RefCell<Option<(u64, Rc<RefCell<WyDerived>>)>>,
    /// 行整形内容缓存（按块下标；键含 `epoch`/`mgen`）：滚动/带重建复用已整形块，
    /// 不再整带重 shape + 代码块重跑 syntect（大文档滚动「每档 65ms」的主因）。
    /// 非结构性编辑只作废被编辑块，结构性/换源/字号变化整体清空。
    shape_rows: RefCell<RowShapeCache>,
    /// 含 `.notdef`（glyph id=0，Windows 冷启动回退未命中）的块 → 自愈状态机。
    /// 见 [`NotdefRetry`] 文档「冷却一帧破 LineLayoutCache」：Pass2 命中/入缓存前都
    /// 扫 .notdef，对坏块走「冷却一帧 → 新鲜平台重排」直至 DWrite 回退解析成功。
    notdef_retry: RefCell<NotdefRetry>,
    /// （仅测试）自上次读取以来 Pass2 实际发生的行整形次数（缓存未命中/光标行）。
    #[cfg(test)]
    reshapes: Cell<u64>,
    /// （仅测试）自上次读取以来 D1 预热实际发生的行整形次数（`prewarm_band` 计数）。
    #[cfg(test)]
    prewarm_shapes: Cell<u64>,
    /// （仅测试）Pass2 notdef 冷却帧数（复用旧几何、未调用 gpui shape）。
    #[cfg(test)]
    coolings: Cell<u64>,
    /// 图片异步加载缓存（gpui `ImageAssetLoader` 实体；命中返回 `Arc<RenderImage>`）。
    image_cache: gpui::Entity<gpui::RetainAllImageCache>,
    /// 独立图片行 src → 逻辑显示尺寸 `(w, h)`（按加载后的宽高比换算）。
    /// `None`(外层) = 尚未尝试加载；`Some(None)` = 加载失败（一直显示 alt 兜底）；
    /// `Some(Some(..))` = 已加载。供 shape（假布局尺寸）与 paint（占位）共用。
    image_sizes: RefCell<std::collections::HashMap<SharedString, Option<(f32, f32)>>>,
    /// 图片加载完成代数：任何图片从「占位尺寸」变成「真实尺寸」，或拖拽改宽，
    /// 自增——纳入帧缓存键，新图/新宽到达后必须重建整帧（旧总高/几何失效）。
    /// **不进基础布局代**（`height_epoch`）：图片行高的变更由 `pending_image_srcs` 收录、
    /// `build_frame` 走稀疏定向覆写，见 [`Self::patch_pending_image_heights`]。
    images_gen: Cell<u64>,
    /// 用户拖拽调整的图片宽（px，按 src）：`None`（地图缺项）= 未调整 → 自然宽
    /// `min(avail, IMAGE_MAX_W)`。值已钳在 `[IMAGE_MIN_W, IMAGE_MAX_W]` 内。
    /// 改变 → `images_gen` 自增（帧/整形缓存失效 → 图片按新宽 + 原宽高比重排）。
    image_widths: RefCell<std::collections::HashMap<SharedString, f32>>,
    /// 「图片 `src` → 引用它的块下标」**反向索引**（M3 A8）：`invalidate_image_row_cache`
    /// 用它 O(1) 定位相关块，替代旧的「当前帧行 + 逐块 `parse_image_line` 全量兜底」。
    /// 键 = `content_gen`，一处构建、全更新点自动同步（同 `toc_cache`）。见
    /// [`ImageSrcBlocks`]。
    image_src_blocks: RefCell<Option<ImageSrcBlocks>>,
    /// 待「基础布局稀疏覆写」（M3 A4）的图片 `src`：`set_image_size`/`set_image_width`
    /// 收录；`build_frame` 在**基表复用**路径按 `avail` 定向 patch 含图块高——图片高变更
    /// 不进 `height_epoch`（免拖拽每 tick 近全量 Pass1），走与 mgen 同理的稀疏增量，每次
    /// build_frame 消费清空。见 [`Self::patch_pending_image_heights`]。
    pending_image_srcs: RefCell<std::collections::HashSet<SharedString>>,
    /// 进行中的图片调整条拖拽（右中边缘）。`Some` = 按住拖动中（随鼠标改宽，不改源码）。
    image_resize_drag: RefCell<Option<ImageResizeDrag>>,
    /// LaTeX → 排版结果缓存（参考 zorite `<MathStore>` 按公式源码做键）。
    /// `Loading` 排版中 / `Ready` 就绪（位图 + 逻辑 px 尺寸）/ `Failed` 失败（回退 Unicode）。
    math_slots: Rc<RefCell<std::collections::HashMap<SharedString, MathSlot>>>,
    /// 排版文字色；主题切换 → 清缓存重排（同 zorite `MathStore::set_color`）。
    math_color: RefCell<Option<Hsla>>,
    /// 排版代数：任何公式 Loading→Ready/Failed 自增，纳入帧缓存键（同 `images_gen`）。
    math_gen: Cell<u64>,
    /// mermaid 图逐块滚轮缩放：键 = 围栏块首源码行。
    mermaid_zoom: std::cell::RefCell<std::collections::HashMap<usize, f32>>,
    /// mermaid 缩放代数：缩放变化自增，纳入帧缓存键 + 基础布局代（同 `math_gen`，强迫重排）。
    mermaid_zoom_gen: Cell<u64>,
    /// 当前获得焦点的 mermaid 卡（`src_line`）：**点击卡片**置位、点击其它处清除。
    /// 仅聚焦的 mermaid 用滚轮缩放（`on_scroll_wheel` 只消费聚焦卡上的滚轮），未聚焦时
    /// 滚轮一律放行给宿主正常滚动——避免「往下滚遇到 mermaid 就缩图」。视图本地态，不进撤销栈。
    focused_mermaid: Option<usize>,
    /// 代码块高亮解析缓存（后台 syntect 整块解析；`request_code_parse` 置 Pending、
    /// `sync_code_parse` 后台跑 `parse_code_lines` 后落 Done）。解析就绪由
    /// `on_code_parse_ready` **定向作废该代码块的行整形缓存条目**（非全局代数）→ 下帧
    /// 重建用高亮行替换占位纯文本行。
    code_parse:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<CodeParseKey, CodeParseState>>>,
    /// 本帧登记的待后台解析队列 `((语言, 代码), 块 src_line)`（`sync_code_parse` 每帧
    /// 清空消费）。`src_line` 用于完成时的**带内门控**（块还在带内才立即重建帧上色）。
    code_parse_need: RefCell<Vec<(CodeParseKey, usize)>>,
    /// 数学显示行数缓存（见字段声明）。
    math_line_map: RefCell<std::collections::HashMap<String, f32>>,
    /// 行内公式键（`"inline:…"`）→ 包含它的行 src_line（`sync_math` claim 时登记）。
    /// `on_math_ready` 完成时**定向作废**这些行整形缓存条目——替代 mgen 进全局 shape_key
    /// 的整带 miss（`$$` 显示块在 Pass2 被 blocked、不入 `shape_rows`，无需作废）。
    /// 换源清空（行号随源失效）；轻微过期只多作废一两个无关块（无害，正确性不受影响）。
    inline_math_rows: RefCell<std::collections::HashMap<SharedString, Vec<usize>>>,
    /// 块级 `$$` 卡「最近一次就绪位图」钉（键 = 块首源码行）：公式源逐键变化使当前键
    /// 落在 Loading/缺位时，paint 用钉住的旧位图代替 Unicode 回退，新位图就绪时以同
    /// 几何无缝替换——消除「Loading Unicode 回退 ↔ Ready 位图」两态闪跳（键入回弹）。
    /// 结构编辑/换源/主题色变化清空（行号或颜色失效）；其余时间的钉是 Op(块数) 的有界
    /// 缓存（每块至多一条，位图 `Arc` 与槽共享）。
    math_pins: RefCell<std::collections::HashMap<usize, BlockMathPin>>,
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
    /// [`crate::config::timing::HIGHLIGHT_MS`] 后自动消失（`highlight_seq` 防旧定时器误清）。
    highlight_search: Option<(usize, String)>,
    /// 高亮代序列号：每次发起/清除高亮 +1，定时器只清除同代高亮。
    highlight_seq: u64,
    /// 代码块横向滚动偏移（px；键 = 代码块首源码行）。长代码行 nowrap 超宽时
    /// 由横向滚动条/拖动驱动（paint 平移 + 命中/光标/选区反向加回）。纯视图状态，
    /// 不写源码、不进撤销栈。
    code_h_scroll: RefCell<std::collections::HashMap<usize, f32>>,
    /// 代码块横向滚动条拖拽进行中：`Some((块首源码行, 指针相对滑块左缘偏移))`。
    code_h_drag: RefCell<Option<(usize, f32)>>,
    /// 表格横向滚动偏移（px；键 = 表格块首源码行）。超宽表格（`total_w` > 内容宽）
    /// 由底部横向滚动条驱动（paint 平移 + 命中/光标反向加回，参考代码块 `code_h_scroll`；
    /// 对齐 preview `markdown/view` 的表格溢出横向滚动）。纯视图状态，不写源码、不进撤销栈。
    table_h_scroll: RefCell<std::collections::HashMap<usize, f32>>,
    /// 表格横向滚动条拖拽进行中：`Some((块首源码行, 指针相对滑块左缘偏移))`。
    table_h_drag: RefCell<Option<(usize, f32)>>,
    /// 数学卡横向滚动偏移（px；键 = 数学块首源码行）。公式自然宽 > 内容盒（长公式
    /// **不缩小**）时由底部横向滚动条/拖动驱动（paint 平移 + 命中反向加回，同
    /// 代码块 `code_h_scroll`）。纯视图状态，不写源码、不进撤销栈。
    math_h_scroll: RefCell<std::collections::HashMap<usize, f32>>,
    /// 数学卡横向滚动条拖拽进行中：`Some((块首源码行, 指针相对滑块左缘偏移))`。
    math_h_drag: RefCell<Option<(usize, f32)>>,
    /// 数学卡内容的**自然宽**侧缓存（px；键 = 数学块首源码行）：paint 每次画卡时
    /// 测好（位图逻辑宽 / Unicode 行最大宽），命中/滚动条几何（`math_hscroll_geo`）
    /// 读它——避免 hit 期无窗口无法排版 Unicode。与滚动偏移同源 `math_h_scroll`。
    math_natural_w: RefCell<std::collections::HashMap<usize, f32>>,
    /// 已折叠的标题区块（视图本地状态，键 = 标题行 trim 后的文本，与 `live` 一致；
    /// markdown 没有标题折叠语法，折叠态**不写回源码**、不进撤销栈）。点击标题行
    /// 右缘的折叠箭头翻转（`▸` 展开 / `▾` 折叠）；折叠后该 section 的 body 行
    /// 折叠成 0 高（光标严格落在 body 行内才 reveal）。
    folded_headings: RefCell<std::collections::HashSet<String>>,
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
    /// 上一个编辑发生的时刻（I1 时间窗）：与它停顿 ≥[`UNDO_GROUP_MS`]（300ms）即新起
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
    /// 图片编辑卡片（打开时在图片旁渲染悬浮表单：链接 + 文本两个输入框）。
    /// `None` = 未打开。打开期间输入框获得焦点（IME 走 `InputState`，与文档输入互不干扰）。
    image_edit: Option<ImageEditPopup>,
    /// 图片编辑卡片的**窗口锚点**（卡片 TopLeft，window 坐标）：paint 期用「本帧
    /// `bounds.origin` + 本帧 frame」同代配对算出并缓存，render 只读它摆卡片。
    /// 绝不能由 render 直接拿 frame/last_bounds 现算——`end_selection` 清帧后
    /// `ensure_frame_for_input` 会用兜底 `px(800)` 宽重建一帧、随后 prepaint 又按真实宽
    /// 重建，render 期每次读到的帧宽不同 → 卡片横向左右跳一帧（「突然往左闪烁」）。
    /// `None` = 无卡片 / 锚点未就绪。
    image_edit_anchor: Cell<Option<gpui::Point<Pixels>>>,
    /// EPUB 注记正文表（`注记号 → 注释文本`，`set_notes` 注入；悬浮卡正文优先读它）。
    note_ctx: std::collections::BTreeMap<usize, SharedString>,
    /// 当前悬浮的注记标记（命中矩形 = 内容列局部，卡锚定用 `content_origin` 换算；
    /// 由 `on_mouse_move`/窗口级 move 更新，注视离开/换源/点击跳转后清除）。
    hover_note: Cell<Option<element::NoteMarkerHit>>,
    /// 引文跳转后给目标行注记标记的**高亮**：`(源码行, 注记号, 是否定义)`，paint 用
    /// accent 淡色画该标记（复用选区几何），[`HIGHLIGHT_MS`] 后自动消失。
    marker_hl: Cell<Option<(usize, usize, bool)>>,
    /// 高亮代序列号（同 `highlight_seq`：定时器只清同代，防旧定时器误清新高亮）。
    marker_hl_seq: Cell<u64>,
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
wy_action!(Backspace, "wy-backspace");
wy_action!(Delete, "wy-delete");
wy_action!(Enter, "wy-enter");
wy_action!(SelectAll, "wy-select-all");
wy_action!(Copy, "wy-copy");
wy_action!(Cut, "wy-cut");
wy_action!(Paste, "wy-paste");
wy_action!(Undo, "wy-undo");
wy_action!(Redo, "wy-redo");

/// 装配键位（入口：`main.rs` 与其它键位一并安装）。
pub fn bind_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
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
        // 图片编辑卡片浮层：在元素树根上**最后**叠加（最后绘制、命中最靠前），
        // 锚定在目标图片旁渲染（打开时才有）。卡片几何取自 paint 期缓存的
        // `image_edit_anchor`（同代 origin+frame），不在此现算帧（避免读到 ensure
        // 兜底 800 宽重建的临时帧而横跳）。
        let image_edit_overlay = self.image_edit_card_element(window, cx);
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
        if let Some(overlay) = image_edit_overlay {
            root = root.child(overlay);
        }
        if let Some(overlay) = note_overlay {
            root = root.child(overlay);
        }
        root
    }
}
#[cfg(test)]
impl WyEditor {
    pub(crate) fn test_theme_fg(&self) -> gpui::Hsla {
        self.theme.text
    }
    pub(crate) fn test_theme_code_bg(&self) -> gpui::Hsla {
        self.theme.code_bg
    }
    pub(crate) fn test_theme_link(&self) -> gpui::Hsla {
        self.theme.link
    }
    pub(crate) fn test_theme_is_dark(&self) -> bool {
        self.theme.is_dark
    }
    /// （仅测试）当前缴存中的编辑日志 op 条数（未开启收集 = 0）。app 回归用实例级
    /// 确定性断言「单字符恰 1 条 / 撤重混用恰 N 条 / 大粘贴单大 op」，不依赖进程级静态。
    pub(crate) fn live_ops_len(&self) -> usize {
        self.live_ops.as_ref().map_or(0, |v| v.len())
    }
}
