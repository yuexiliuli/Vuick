//! WYSIWYG 宿主 `WyEditor` 的**渲染模型状态**（`WyRenderModel`）。
//!
//! 自 `crates/markdown_wysiwyg` 的 `wysiwyg/mod.rs`（`WyEditor` 分散字段）收敛：凡「只被
//! frame/shape/paint/hit（下一轮迁来引擎）消费 + 失效由文档编辑触发」的状态统一装进本
//! 结构体，宿主 `WyEditor` 只留单一 `rd: RefCell<WyRenderModel>` 字段并迁移全部访问点。
//! 本轮是**状态再组织**：不改任何渲染/失效语义，只改字段寻址方式。
//!
//! 同时承载随状态一并迁入的三个纯数据容器（`WyDerived` / `PrebuildPass1` /
//! `BlockMathPin` / `ImageSrcBlocks`）——它们是 `WyRenderModel` 字段的**类型**，引擎不
//! 能反向依赖宿主 `types.rs`，故作为纯类型（无逻辑）随迁（纯结构移动，宿主 `types.rs`
//! 经重导出保持既有路径）。编辑/宿主语义态（doc/caret/selection/cache_host/undo/IME/鼠标
//! 交互等）仍留宿主，见宿主侧字段归属表。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{RenderImage, SharedString};

use crate::cache::{NotdefRetry, RowShapeCache};
use crate::prewarm::Prewarm;
use crate::theme::WyTheme;

/// 一次「内容派生」的全篇扫描结果。纯 `content` 函数 → 按 `regions_ver` 缓存，滚动 /
/// 可视带移动（内容不变）直接复用 `Rc`，**不重扫全篇**——大文档每帧卡顿的主因之一
/// （`math_regions` 7MB 全篇扫描 ~400ms、`alert_*` ~30ms，随每帧 paint / 每档 rebuild
/// 重复执行）。
pub struct WyDerived {
    /// `$$…$$` 数学块区域（首源码行 → 卡片）。
    pub math_regions: Vec<markdown::parse::inline_style::MathRegion>,
    /// 可折叠 callout 区域 `(行区间, 是否折叠)`。
    pub alert_fold_regions: Vec<(Range<usize>, bool)>,
    /// 每个警报区域的 `AlertKind`（marker + body 行），paint 引述条着色用。
    /// Fx 哈希：结构编辑 `shift_derived` 全量平移重插时省 SipHash（大文档几百到
    /// 数千条 ≈ Enter 路径 1-2ms）。
    pub alert_region_kinds:
        rustc_hash::FxHashMap<usize, markdown::parse::recognition::AlertKind>,
    /// **折叠标题体**（M5-B3-1 子项 4 移入 derived，不再每键 `text_shared()` 全篇物化 +
    /// `heading_fold_regions` 全扫）：`fold_plan` 的 `FoldPlanSets` 直接读它。不过它是
    /// **视图折叠态相关**（不属纯 content 函数）——三处同步保一致：
    /// ① `derived()` 重建（`regions_ver` 变）时按当前 `folded_headings` 重扫；
    /// ② `toggle_heading_fold_at_line` 翻转折叠态后就地重扫（稀有用户动作，允许一次 O(n)）；
    /// ③ `shift_derived_after_edit` 结构编辑行号平移时与数学/警报区域同规则平移（编辑线
    /// 落在体**内** → 回退整篇重建，见该函数）。
    pub heading_fold_bodies: Vec<Range<usize>>,
}

/// 首帧基础布局预建输入：每个块只留 `pass1_base_layout::est_block` **除主题/宽度
/// 之外的全部输入**（Meta 形态 + 行长/ASCII + 围栏类特殊源文本）。全部与 doc 同源
/// 计算、`Send`，可在后台线程构建。
/// 主题/宽度相关（`estimate_height` 的行高乘子、折行宽 cpl、图片占位高、mermaid 卡高
/// 常数）留到首帧 `pass1_from_prebuild` 以真实 `WyTheme`/`avail` 现场折算——因为
/// 后台 `open_from_rope` 无窗口/主题（不引入 app 侧改动的前提下此边界不可逾越）。
#[derive(Default)]
pub struct PrebuildPass1 {
    pub blocks: Vec<PrebuildBlock>,
}

impl PrebuildPass1 {
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// 单个块的预建估算输入（≈32B/块；160k 块 ≈ 5MB，仅跨一次后台 spawn 后即转
/// `BlockLayout` 释放）。
#[derive(Debug, Clone)]
pub struct PrebuildBlock {
    pub src_line: u32,
    pub kind: markdown::parse::RowKind,
    /// Code：代码行数；Table：数据行数；Image：1；其余 0。
    pub aux: u32,
    pub chars: u32,
    pub ascii: bool,
    /// 该围栏块的 mermaid 源码（` ```mermaid` 开头的 Code 块才 Some；卡高与
    /// mermaid 缩放无关部分在后台先算好，首帧再乘 zoom——冷开守卫 zgen==0 → zoom=1）。
    pub mermaid: Option<gpui::SharedString>,
    /// 该图片块的解析 src（`parse_image_line` 结果）；首帧按当前 `image_sizes` 现场
    /// 走 `image_block_base_height`（与 `pass1_base_layout`/稀疏覆写同一公式）。
    pub image_src: Option<gpui::SharedString>,
}

/// 一块 `$$` 卡最近一次**就绪位图**的钉（paint/sync_math 在 Ready 时录制；当前键
/// Loading/缺位时改用它，防止 Unicode 回退闪跳）。尺寸与槽一致（位图逻辑 px）。
#[derive(Clone)]
pub struct BlockMathPin {
    /// 位图 + 逻辑（pre-DPR）px 尺寸。
    pub image: Arc<RenderImage>,
    pub w: f32,
    pub h: f32,
}

/// M3 A8：「图片 `src` → 引用它的块下标」**反向索引**条目（`content_gen` + 表）——
/// `invalidate_image_row_cache` 用它 O(1) 定位相关块，替代缺帧/图不在带内时的旧全量
/// `parse_image_line` 兜底扫描（O(块数)）。键 = `content_gen`：内容任一变更即视为陈旧，
/// 下次失效调用整表重建——一处构建、全更新点经 `content_gen` 增量自动同步（对齐
/// `TocCacheEntry` 的派生缓存纪律，避免散落多点手动同步漂移）。
pub type ImageSrcBlocks = (u64, HashMap<SharedString, Vec<usize>>);

/// 行内 `![alt](src)` 的稳定标识（悬浮/选中态用）。`display_off` 是同帧内行内唯一键
/// （paint/命中比较用）；`start` = 该图 `![…](…)` 起始 `!` 的**源字节**（源码系身份，
/// 编辑/删除/保存时从源码反解区间，不依赖帧）。自宿主 `wysiwyg/types.rs` 随迁（纯
/// 结构移动）：`WyRenderHost::hover_inline_image`/`selected_inline_image` 的返回类型，
/// 引擎不能反向依赖宿主。宿主 `types.rs` 经 re-export 保持既有路径。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InlineImageRef {
    /// 所在源码行。
    pub src_line: usize,
    /// 显示偏移（`InlineImageHit.display_off`）。
    pub display_off: usize,
    /// `![…](…)` 起始 `!` 的源字节偏移。
    pub start: usize,
}

/// 三段横向滚动条（代码块 / 表格 / 数学卡）的统一 kind。P3 / Slice C 收敛：原定义在
/// wysiwyg `mouse.rs` 的局部枚举上移为引擎模型类型，连同 [`HScrollState`] 状态桶一起，
/// 消费面（view/mouse/hit）统一经 kind 索引。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HScrollKind {
    Code,
    Table,
    Math,
}

impl HScrollKind {
    /// 全部 kind（拖拽轮询 / 松开清槽按序使用，次序 = 拆分前三段 if 顺序）。
    pub const ALL: [HScrollKind; 3] = [Self::Code, Self::Table, Self::Math];
}

/// 单个 kind 的横向滚动状态桶：`scroll` = 滚动偏移表（键 = 块首源码行），`drag` = 拖拽槽
/// （`Some((块首源码行, 指针相对滑块左缘偏移))`）。纯视图状态，不写源码、不进撤销栈。
/// P3 / Slice C：由原 `{code,table,math}_h_scroll` / `{code,table,math}_h_drag` 各二字段收敛。
pub struct HScrollBucket {
    pub scroll: RefCell<HashMap<usize, f32>>,
    pub drag: RefCell<Option<(usize, f32)>>,
}

impl HScrollBucket {
    fn new() -> Self {
        Self {
            scroll: RefCell::new(HashMap::new()),
            drag: RefCell::new(None),
        }
    }
}

/// 代码/表格/数学三段横向滚动状态（code / table / math 各一桶）。P3 / Slice C 收敛：
/// setter 阈值（`v=max(0.)`、`<=0.01` 删键）与窗口 retain 语义逐一保留在此单一实现。
pub struct HScrollState {
    pub code: HScrollBucket,
    pub table: HScrollBucket,
    pub math: HScrollBucket,
}

impl Default for HScrollState {
    fn default() -> Self {
        Self::new()
    }
}

impl HScrollState {
    /// 全新建一份（与迁移前各 `RefCell` 初值逐字一致）。
    pub fn new() -> Self {
        Self {
            code: HScrollBucket::new(),
            table: HScrollBucket::new(),
            math: HScrollBucket::new(),
        }
    }

    fn bucket(&self, k: HScrollKind) -> &HScrollBucket {
        match k {
            HScrollKind::Code => &self.code,
            HScrollKind::Table => &self.table,
            HScrollKind::Math => &self.math,
        }
    }

    /// 读某 kind 的滚动偏移（缺键 = 0）。
    pub fn scroll_of(&self, k: HScrollKind, src: usize) -> f32 {
        self.bucket(k).scroll.borrow().get(&src).copied().unwrap_or(0.)
    }

    /// 写某 kind 的滚动偏移（**唯一 setter 实现**）：`<0` 归一 0，`<=0.01` 删键否则插键。
    pub fn set_scroll(&self, k: HScrollKind, src: usize, v: f32) {
        let v = v.max(0.);
        let mut m = self.bucket(k).scroll.borrow_mut();
        if v <= 0.01 {
            m.remove(&src);
        } else {
            m.insert(src, v);
        }
    }

    /// 写 / 清某 kind 的拖拽槽。
    pub fn set_drag(&self, k: HScrollKind, v: Option<(usize, f32)>) {
        *self.bucket(k).drag.borrow_mut() = v;
    }

    /// 读某 kind 的拖拽槽。
    pub fn drag_of(&self, k: HScrollKind) -> Option<(usize, f32)> {
        *self.bucket(k).drag.borrow()
    }

    /// 结构编辑后清除「`edit_line..=edit_line+max_nl` 行窗」内以 src 为键的三段横向滚动
    /// 偏移（窗口 retain 语义与次序同拆分前 `clear_src_line_view_state`；`math_natural_w` /
    /// `mermaid_zoom` 的 retain 仍在宿主侧同函数保留）。
    pub fn retain_src_line_window(&self, edit_line: usize, max_nl: usize) {
        let end = edit_line.saturating_add(max_nl);
        for k in HScrollKind::ALL {
            let mut m = self.bucket(k).scroll.borrow_mut();
            m.retain(|&l, _| l < edit_line || l > end);
        }
    }
}

/// WYSIWYG 宿主 `WyEditor` 的**渲染模型状态**（单一租户：宿主持 `RefCell<WyRenderModel>`
/// 并迁移全部访问点为 `self.rd.borrow()/.borrow_mut()`）。
///
/// 归属准则（与宿主侧分工）：凡「只被 frame/shape/paint/hit（下一轮迁来引擎）消费 +
/// 失效由文档编辑触发」的状态 → 本结构体；凡「宿主事件/编辑逻辑直接读写且引擎不需要」
/// （doc/caret/selection/cache_host/undo/IME/鼠标交互/宿主滚动句柄等）→ 留宿主。本轮不改
/// 任何渲染/失效语义，只看字段寻址方式变化。
pub struct WyRenderModel {
    /// 主题快照（`WyTheme` 纯类型；`set_theme`/`set_font_size`/`rebuild_theme_from_global`
    /// 经 `host.rd.borrow_mut().theme` 覆写，并同步失效帧/布局/整形缓存）。
    pub theme: WyTheme,
    /// 本帧行网格（measure→prepaint→paint 复用；**窗口化**：仅带内块被真正 shape，
    /// 带外块用 `BlockMeta` 估算高度占位，避免大文档整篇每帧 shape 卡死）。
    pub frame: RefCell<Option<Rc<crate::element::Frame>>>,
    /// 内容代数：任何文本变更自增；`request_layout` 用它判断帧缓存是否仍有效。
    pub content_gen: Cell<u64>,
    /// 可视带（窗口坐标→元素局部 y，量程化）：prepaint 用 `bounds.origin.y` +
    /// `viewport_size` 推算并发布，measure 用上一帧的带做窗口化 shape。
    pub shape_band: Cell<Option<(f32, f32)>>,
    /// **滚动预热**状态（`element::prewarm_band`）：把下一量子带块提前 shape 进
    /// `shape_rows` 缓存（键与 band 无关）→ 带穿越的 6.4ms/band shape 突发被摊平。
    /// 方向由带序号跨界判定；结构编辑清空整形缓存后必须归零（见 `replace_range`）。
    pub prewarm: RefCell<Prewarm>,
    /// 全篇**基础布局**缓存：`(代, Rc<RefCell<BlockLayout>>)`——rolling/带移动与
    /// **非结构性编辑**都复用（只对受影响块做 Fenwick 点更新），代 ≠ 内容代：
    /// 由 `struct_ver`/宽度/mermaid 缩放组成（**不含 `images_gen`/`mgen`**——图片/数学高
    /// 变更走稀疏定向覆写 / `fold_plan` 稀疏 delta，见 `height_epoch` eval）。结构性变更
    /// （换行/围栏/表格等改了块数或块序）才整篇重建。`set_theme`/`set_font_size` 把代置
    /// 失效位强制重算。
    pub block_layout: RefCell<Option<(u64, Rc<RefCell<crate::element::BlockLayout>>)>>,
    /// 首帧基础布局**预建输入槽**（Step4-①）：`install_open` 存 `WyOpenResult.prebuild`，
    /// 首个 `build_frame`（`rebuilt` 分支）经 `frame::pass1_from_prebuild` **一次性**
    /// 用真实主题/宽度物化成 `BlockLayout` 后清空——首帧不再全量 doc 估高。任何编辑
    /// （`struct_ver > 0`）/mermaid 缩放（`zgen != 0`）/块数不齐 → 守卫放行走既有
    /// `pass1_base_layout` 兜底（正确优先）。
    pub prebuild_slot: RefCell<Option<PrebuildPass1>>,
    /// 内容**结构版本**：`replace_range` 里结构性变更（跨行、代码/表格边界）才递增——
    /// 参与基础布局代；非结构性编辑不递增（基础布局跨键复用，只 patch 受影响块高）。
    pub struct_ver: Cell<u64>,
    /// 结构性编辑影响的**首个源行**（Pass1 增量高度重估的起点）：编辑线之前的块
    /// 内容未变 → 复用旧布局高度；只有「编辑线起 → 文尾」重新估算（Enter 贴近文尾时
    /// O(变化后缀) 而非全篇）。非结构编辑不参与（无重建）。
    pub struct_dirty_line: Cell<usize>,
    /// 编辑影响的**最后一个旧源行**（`edit_line + 被删换行数`）：Pass1 用它定位旧尾部
    /// 起点（`j`），从而**复用尾部高度**（编辑下方块内容未变，高度不变）——把结构编辑
    /// 从「dirty_line→文尾」全后缀重估降为「仅变化中段」+ 尾部 Vec 复制。
    pub struct_old_end_line: Cell<usize>,
    /// 编辑的源行净增（插入换行数 − 被删换行数）：尾部块 src_line 整体平移量。
    pub struct_line_delta: Cell<i64>,
    /// 结构性编辑被编辑块的**旧块下标**（编辑前）与**块数净增**（编辑后 − 编辑前）：
    /// `fold_plan` deltas 增量平移用（块下标 ≥ 旧块下标之后整体 + 块数净增）。
    pub struct_blk_old: Cell<usize>,
    pub struct_blk_span_delta: Cell<i64>,
    /// 结构性编辑待消费标志：结构性分支置位、`fold_plan` 消费后清除——防止增量平移
    /// 误用陈旧编辑信息（非结构编辑/宽度变化恰在结构编辑之后、其间无重建时）。
    pub struct_edit_pending: Cell<bool>,
    /// 结构编辑「行号平移」版本（M5-B3-1 子项 2/4）：`shift_derived_after_edit` 成功
    /// 就地平移区域行号（结构不变、只是行号动）时递增——纳入 `fp_sets` 缓存键，
    /// 防集合命中旧行号；`regions_ver++` 整篇重建走另一条键分量（`regions_ver`）天然
    /// 失效。
    pub regions_shift_ver: Cell<u64>,
    /// 内容**派生版本**——`derived()` 缓存键：结构性编辑、编辑行 region 签名变化才
    /// 递增；纯文本键入（非结构、签名不变、不在数学块内）不递增 → `derived` Rc
    /// 跨键复用，不再每键对全篇 `math_regions`/`alert_*` 重扫（7MB/结构密集文档
    /// 「输入卡死」的主因，旧实现每键 2 次全篇扫描 ≈1s）。
    pub regions_ver: Cell<u64>,
    /// 折叠/重排计划缓存：`(key, marker_key, Rc<FoldPlan>)`——与可视带无关（内容、caret
    /// reveal、数学就绪、宽度、结构版本决定），滚动/带移动复用；build_frame 每档不再
    /// 重建 ~万行 Hash 集合 + 逐块二分（大文档滚动「卡顿明显」：~40ms/档）。
    /// `marker_key` = regions_ver^mgen^fold_ver：marker 身份在结构编辑间未变 → deltas
    /// 可增量平移复用（`fold_plan_incremental`）。
    pub fold_plan: RefCell<Option<(u64, u64, Rc<crate::element::FoldPlan>)>>,
    /// 折叠计划 **marker 集合**缓存：`(sets_key, Rc<FoldPlanSets>)`。`sets_key` =
    /// `regions_ver ^ regions_shift_ver ^ fold_ver`（只 region/折叠结构，**不含布局
    /// epoch / mgen**）——布局换代、宽度/字号、公式光栅就绪（mgen）只改 `deltas`（高度），
    /// marker 集合跨计划复用（M5-B3-1 子项 2，免每次 O(#区域) 哈希重建）。
    pub fp_sets: RefCell<Option<(u64, Rc<crate::element::FoldPlanSets>)>>,
    /// 标题折叠视图态版本：`toggle_heading_fold_at_line` 翻转 `folded_headings` 时递增
    /// （纳入 `fold_plan` 缓存键，滚动不带折叠态重算）。
    pub fold_ver: Cell<u64>,
    /// `$` 出现次数（内容级增量计数；`derived` 的数学预筛用，免全篇重扫）。
    pub math_dollar_cnt: Cell<usize>,
    /// `[!` 出现次数（`derived` 的警报/callout 预筛用，免全篇重扫）。
    pub alert_marker_cnt: Cell<usize>,
    /// 最近一次编辑受影响（需重估高度）的块下标；`build_frame` 消费后清空。
    /// `None` = 无（或结构性编辑 → 整篇重建，不进此表）。
    pub pending_patch_blocks: RefCell<Option<Vec<usize>>>,
    /// 内容派生扫描缓存：`(regions_ver, Rc<RefCell<WyDerived>>)`——滚动/带移动与纯文本
    /// 键入复用；结构性/签名变化重建。`RefCell` 供编辑期对数学块 `source` 做局部修补
    /// （`replace_range`，避免键入大文档数学块内部也整篇重建）。
    pub derived: RefCell<Option<(u64, Rc<RefCell<WyDerived>>)>>,
    /// 行整形内容缓存（按块下标；键含 `epoch`/`mgen`）：滚动/带重建复用已整形块，
    /// 不再整带重 shape + 代码块重跑 syntect（大文档滚动「每档 65ms」的主因）。
    /// 非结构性编辑只作废被编辑块，结构性/换源/字号变化整体清空。
    pub shape_rows: RefCell<RowShapeCache>,
    /// 含 `.notdef`（glyph id=0，Windows 冷启动回退未命中）的块 → 自愈状态机。
    /// 见 `NotdefRetry` 文档「冷却一帧破 LineLayoutCache」：Pass2 命中/入缓存前都
    /// 扫 .notdef，对坏块走「冷却一帧 → 新鲜平台重排」直至 DWrite 回退解析成功。
    pub notdef_retry: RefCell<NotdefRetry>,
    /// （仅测试用宿主计数器）自上次读取以来 Pass2 实际发生的行整形次数（缓存未命中/
    /// 光标行）。⚠️ 不能 `#[cfg(test)]`：宿主 `markdown_wysiwyg` 的测试依赖的是引擎的
    /// **lib 构建**（引擎自身的 `cfg(test)` 不传播），宿主测试经 `rd` 读它必须恒在场。
    pub reshapes: Cell<u64>,
    /// （仅测试用宿主计数器）自上次读取以来 D1 预热实际发生的行整形次数（`prewarm_band`
    /// 计数）。同上不可 cfg(test)。
    pub prewarm_shapes: Cell<u64>,
    /// （仅测试用宿主计数器）Pass2 notdef 冷却帧数（复用旧几何、未调用 gpui shape）。
    /// 同上不可 cfg(test)。
    pub coolings: Cell<u64>,
    /// 独立图片行 src → 逻辑显示尺寸 `(w, h)`（按加载后的宽高比换算）。
    /// `None`(外层) = 尚未尝试加载；`Some(None)` = 加载失败（一直显示 alt 兜底）；
    /// `Some(Some(..))` = 已加载。供 shape（假布局尺寸）与 paint（占位）共用。
    pub image_sizes: RefCell<HashMap<SharedString, Option<(f32, f32)>>>,
    /// 图片加载完成代数：任何图片从「占位尺寸」变成「真实尺寸」，或拖拽改宽，
    /// 自增——纳入帧缓存键，新图/新宽到达后必须重建整帧（旧总高/几何失效）。
    /// **不进基础布局代**（`height_epoch`）：图片行高的变更由 `pending_image_srcs` 收录、
    /// `build_frame` 走稀疏定向覆写，见宿主 `patch_pending_image_heights`。
    pub images_gen: Cell<u64>,
    /// 用户拖拽调整的图片宽（px，按 src）：`None`（地图缺项）= 未调整 → 自然宽
    /// `min(avail, IMAGE_MAX_W)`。值已钳在 `[IMAGE_MIN_W, IMAGE_MAX_W]` 内。
    /// 改变 → `images_gen` 自增（帧/整形缓存失效 → 图片按新宽 + 原宽高比重排）。
    pub image_widths: RefCell<HashMap<SharedString, f32>>,
    /// 「图片 `src` → 引用它的块下标」**反向索引**（M3 A8）：宿主
    /// `invalidate_image_row_cache` 用它 O(1) 定位相关块，替代旧的「当前帧行 + 逐块
    /// `parse_image_line` 全量兜底」。键 = `content_gen`，一处构建、全更新点自动同步
    /// （同 `toc_cache`）。
    pub image_src_blocks: RefCell<Option<ImageSrcBlocks>>,
    /// 待「基础布局稀疏覆写」（M3 A4）的图片 `src`：宿主 `set_image_size`/
    /// `set_image_width` 收录；`build_frame` 在**基表复用**路径按 `avail` 定向 patch 含图块
    /// 高——图片高变更不进 `height_epoch`（免拖拽每 tick 近全量 Pass1），走与 mgen 同理的
    /// 稀疏增量，每次 build_frame 消费清空。
    pub pending_image_srcs: RefCell<HashSet<SharedString>>,
    /// 排版代数：任何公式 Loading→Ready/Failed 自增，纳入帧缓存键（同 `images_gen`）。
    pub math_gen: Cell<u64>,
    /// mermaid 图逐块滚轮缩放：键 = 围栏块首源码行。
    pub mermaid_zoom: RefCell<HashMap<usize, f32>>,
    /// mermaid 缩放代数：缩放变化自增，纳入帧缓存键 + 基础布局代（同 `math_gen`，强迫重排）。
    pub mermaid_zoom_gen: Cell<u64>,
    /// 块级 `$$` 卡「最近一次就绪位图」钉（键 = 块首源码行）：公式源逐键变化使当前键
    /// 落在 Loading/缺位时，paint 用钉住的旧位图代替 Unicode 回退，新位图就绪时以同
    /// 几何无缝替换——消除「Loading Unicode 回退 ↔ Ready 位图」两态闪跳（键入回弹）。
    /// 结构编辑/换源/主题色变化清空（行号或颜色失效）；其余时间的钉是 Op(块数) 的有界
    /// 缓存（每块至多一条，位图 `Arc` 与槽共享）。
    pub math_pins: RefCell<HashMap<usize, BlockMathPin>>,
    /// 代码/表格/数学三段横向滚动状态（滚动偏移表 + 拖拽槽，按 [`HScrollKind`] 分桶）。
    /// P3 / Slice C：原 6 个 `{code,table,math}_h_scroll`/`{code,table,math}_h_drag` 字段
    /// 收敛为 [`HScrollState`]（setter 阈值 / 窗口 retain 语义逐一保留）。纯视图状态，
    /// 不写源码、不进撤销栈。
    pub h_scroll: HScrollState,
    /// 数学卡内容的**自然宽**侧缓存（px；键 = 数学块首源码行）：paint 每次画卡时
    /// 测好（位图逻辑宽 / Unicode 行最大宽），命中/滚动条几何（宿主 `math_hscroll_geo`）
    /// 读它——避免 hit 期无窗口无法排版 Unicode。与滚动偏移同源 `math_h_scroll`。
    pub math_natural_w: RefCell<HashMap<usize, f32>>,
    /// 已折叠的标题区块（视图本地状态，键 = 标题行 trim 后的文本，与 `live` 一致；
    /// markdown 没有标题折叠语法，折叠态**不写回源码**、不进撤销栈）。点击标题行
    /// 右缘的折叠箭头翻转（`▸` 展开 / `▾` 折叠）；折叠后该 section 的 body 行
    /// 折叠成 0 高（光标严格落在 body 行内才 reveal）。
    pub folded_headings: RefCell<HashSet<String>>,
    /// 引文跳转后给目标行注记标记的**高亮**：`(源码行, 注记号, 是否定义)`，paint 用
    /// accent 淡色画该标记（复用选区几何），`HIGHLIGHT_MS` 后自动消失。
    pub marker_hl: Cell<Option<(usize, usize, bool)>>,
    /// 高亮代序列号（同宿主 `highlight_seq`：定时器只清同代，防旧定时器误清新高亮）。
    pub marker_hl_seq: Cell<u64>,
    /// 数学**显示行数→卡高估计**缓存（键=源）：`math_line_height_h` 对每源做
    /// LaTeX→Unicode 转换与高算子扫描，3000 块每次 `fold_plan` 重建重复 ≈38ms；
    /// 按源缓存后 O(1) 复用（同一源估计值确定不变）。
    pub math_line_map: RefCell<HashMap<String, f32>>,
    /// 撞过 `config::limits::MATH_LINE_MAP_CAP` 的**一次性告警节流**（换源 clear 时复位，
    /// 避免超限后每个新源都刷一条 stderr；只提示一次让超限可见）。
    pub math_line_map_warned: Cell<bool>,
}

impl WyRenderModel {
    /// 全新建一份渲染模型（宿主 `WyEditor::new` 调用；字段初值与迁移前逐字一致）。
    pub fn new(theme: WyTheme) -> Self {
        Self {
            theme,
            frame: RefCell::new(None),
            content_gen: Cell::new(0),
            shape_band: Cell::new(None),
            prewarm: RefCell::new(Prewarm::default()),
            block_layout: RefCell::new(None),
            prebuild_slot: RefCell::new(None),
            struct_ver: Cell::new(0),
            struct_dirty_line: Cell::new(0),
            struct_old_end_line: Cell::new(0),
            struct_line_delta: Cell::new(0),
            struct_blk_old: Cell::new(0),
            struct_blk_span_delta: Cell::new(0),
            struct_edit_pending: Cell::new(false),
            regions_shift_ver: Cell::new(0),
            regions_ver: Cell::new(0),
            fold_plan: RefCell::new(None),
            fp_sets: RefCell::new(None),
            fold_ver: Cell::new(0),
            math_dollar_cnt: Cell::new(0),
            alert_marker_cnt: Cell::new(0),
            pending_patch_blocks: RefCell::new(None),
            derived: RefCell::new(None),
            shape_rows: RefCell::new(RowShapeCache::new(
                config::limits::SHAPED_ROW_CACHE_CAP,
            )),
            notdef_retry: RefCell::new(NotdefRetry::default()),
            reshapes: Cell::new(0),
            prewarm_shapes: Cell::new(0),
            coolings: Cell::new(0),
            image_sizes: RefCell::new(HashMap::new()),
            images_gen: Cell::new(0),
            image_widths: RefCell::new(HashMap::new()),
            image_src_blocks: RefCell::new(None),
            pending_image_srcs: RefCell::new(HashSet::new()),
            math_gen: Cell::new(0),
            mermaid_zoom: RefCell::new(HashMap::new()),
            mermaid_zoom_gen: Cell::new(0),
            math_pins: RefCell::new(HashMap::new()),
            h_scroll: HScrollState::new(),
            math_natural_w: RefCell::new(HashMap::new()),
            folded_headings: RefCell::new(HashSet::new()),
            marker_hl: Cell::new(None),
            marker_hl_seq: Cell::new(0),
            math_line_map: RefCell::new(HashMap::new()),
            math_line_map_warned: Cell::new(false),
        }
    }
}
