use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use gpui::{RenderImage, SharedString};
use crate::markdown::doc::Doc;
use crate::markdown::wysiwyg::element;

/// 块布局的可变引用（`ensure_block_layout` 返回 / 缓存槽共用）。
pub(crate) type BlockLayoutRef = Rc<std::cell::RefCell<element::BlockLayout>>;
/// A4：标题目录条目 `(源码行, 标题级别, 标题文本)` 容器（WyEditor 目录 / 侧边栏共用）。
pub(crate) type TocEntries = Vec<(usize, u8, SharedString)>;
/// A4：目录缓存条目（`content_gen` + 条目；侧边栏目录每帧渲染消费用）。
pub(crate) type TocCacheEntry = (u64, TocEntries);
/// 撤销/重做单元：**精确逆操作**（整组净一次编辑）——撤销/重做走 `replace_range` 同款
/// 增量管道（`Doc::edit` 增量块扫描 + Pass1 三段式 + derived 小修补 + 单块整形作废），
/// 不再像旧快照那样整体重建模型（`Doc::open_rope` 全量重索引 + 全毁派生/布局 → 实测
/// 6.4MB 撤销 ~467ms/次：恢复 355ms + build_frame pass1 全量重估 88ms）。
/// `insert` 当前占据 `range`；`delete` 是撤销时要换回的原文（对应单元前状态）。
/// B2 事务合并：连续同意图且邻接的编辑并入同一单元（`insert`/`delete` 累积成净一次替换）。
#[derive(Clone)]
pub(crate) struct UndoUnit {
    /// 当前文档中 `insert` 所占区间（撤销 = 把它替换回 `delete`）。
    pub(crate) range: Range<usize>,
    /// 该单元净插入（当前在 `range` 处）。
    pub(crate) insert: String,
    /// 该单元前该区间原本的文本（撤销后回到它）。
    pub(crate) delete: String,
    /// 单元起始光标（撤销恢复位）。
    pub(crate) caret_before: usize,
    /// 前向恢复光标（**按下撤销瞬间**的光标；`undo` 写入、`redo` 读取，同行 `editor/undo.rs`）。
    pub(crate) forward_caret: usize,
}
/// 编辑意图（决定撤销合并规则，纯无定时器确定性判定，同 editor/undo.rs）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditKind {
    /// 纯字符插入（无换行、邻接续打 → 合并）。
    Typing,
    /// 退格（纯删除、首尾相接 → 合并）。
    Backspace,
    /// 正向删除（纯删除、同起点延伸 → 合并）。
    Delete,
    /// 原子编辑（Enter/Tab/粘贴/程序化写/任务框/折叠 → 永不合并并断层）。
    Atomic,
}
/// 可编辑块区域（与块级编辑映射配套；围栏行不可落点）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockRegion {
    /// 代码围栏：内容行 `lo..=hi` 连续；围栏行 = `lo-1`（开）、`hi+1`（闭）。
    Code { lo: usize, hi: usize },
    /// GFM 表格：表头行 + 数据行 `dlo..=dhi`；分隔行（围栏）= `dlo-1`。
    Table {
        header: usize,
        dlo: usize,
        dhi: usize,
    },
}
/// 图片编辑卡片：两个单行输入框（链接 + 文本）锚定在目标图片旁。
pub(crate) struct ImageEditPopup {
    /// 目标图片行的源码行号。
    pub(crate) src_line: usize,
    /// 图片链接（`![alt](src)` 的 src）输入框。
    pub(crate) link: gpui::Entity<crate::ui::input::InputState>,
    /// 图片文本（alt / 题注）输入框。
    pub(crate) text: gpui::Entity<crate::ui::input::InputState>,
}
/// 图片调整条拖拽：按住右中边缘的条，随鼠标横向移动改图片宽（保持宽高比）。
/// `grab_x` = 按下时指针的窗口 x；拖动中新宽 = `start_w + (mouse_x − grab_x)`。
#[derive(Clone)]
pub(crate) struct ImageResizeDrag {
    /// 目标图片的 src。
    pub(crate) src: gpui::SharedString,
    /// 按下瞬间的图片显示宽（px）。
    pub(crate) start_w: f32,
    /// 按下瞬间指针的窗口 x（px）。
    pub(crate) grab_x: f32,
}
/// 一个 LaTeX 公式的排版产物（键 = 公式源码 LaTeX；参考 zorite 的 `MathStore::Slot`）。
#[derive(Clone)]
pub(crate) enum MathSlot {
    /// 排版已开始，位图未回（paint 回退 Unicode 文本；结束自增 `math_gen`）。
    Loading,
    /// 已就绪：位图 + 逻辑（pre-DPR）px 尺寸。
    Ready(Arc<RenderImage>, f32, f32),
    /// 排版失败（回退 Unicode 文本，不再重试）。
    Failed,
}
/// 一块 `$$` 卡最近一次**就绪位图**的钉（paint/sync_math 在 Ready 时录制；当前键
/// Loading/缺位时改用它，防止 Unicode 回退闪跳）。尺寸与槽一致（位图逻辑 px）。
#[derive(Clone)]
pub(crate) struct BlockMathPin {
    /// 位图 + 逻辑（pre-DPR）px 尺寸。
    pub image: Arc<RenderImage>,
    pub w: f32,
    pub h: f32,
}
/// 行内（`$…$`）公式的排版槽键：与块公式（键 = latex 原样）分开，行内用紧凑
/// `INLINE_PAD` 光栅（否则小公式也被 PAD 顶高、撑行）。`latex` 从不以 `inline:` 起头，
/// 不冲突。
pub(crate) fn inline_math_key(latex: &str) -> SharedString {
    format!("inline:{latex}").into()
}
/// M3 A8：「图片 `src` → 引用它的块下标」**反向索引**条目（`content_gen` + 表）——
/// `invalidate_image_row_cache` 用它 O(1) 定位相关块，替代缺帧/图不在带内时的旧全量
/// `parse_image_line` 兜底扫描（O(块数)）。键 = `content_gen`：内容任一变更即视为陈旧，
/// 下次失效调用整表重建——一处构建、全更新点经 `content_gen` 增量自动同步（对齐
/// [`TocCacheEntry`] 的派生缓存纪律，避免散落多点手动同步漂移）。
pub(crate) type ImageSrcBlocks = (
    u64,
    std::collections::HashMap<SharedString, Vec<usize>>,
);
/// 一次「内容派生」的全篇扫描结果。纯 `content` 函数 → 按 `content_gen` 缓存，滚动 /
/// 可视带移动（内容不变）直接复用 `Rc`，**不重扫全篇**——大文档每帧卡顿的主因之一
/// （`math_regions` 7MB 全篇扫描 ~400ms、`alert_*` ~30ms，随每帧 paint / 每档 rebuild
/// 重复执行）。
pub(crate) struct WyDerived {
    /// `$$…$$` 数学块区域（首源码行 → 卡片）。
    pub math_regions: Vec<crate::markdown::parse::inline_style::MathRegion>,
    /// 可折叠 callout 区域 `(行区间, 是否折叠)`。
    pub alert_fold_regions: Vec<(Range<usize>, bool)>,
    /// 每个警报区域的 `AlertKind`（marker + body 行），paint 引述条着色用。
    /// Fx 哈希：结构编辑 `shift_derived` 全量平移重插时省 SipHash（大文档几百到
    /// 数千条 ≈ Enter 路径 1-2ms）。
    pub alert_region_kinds:
        rustc_hash::FxHashMap<usize, crate::markdown::parse::recognition::AlertKind>,
}
/// 一次全篇的行内公式清单扫描 `(源码行, 槽键)`——已被「可见带懒光栅」取代（`sync_math`
/// 改从窗口化 `frame.rows` 拿带内公式，见约定）。此函数保留一则可应急回退。
pub(crate) fn alert_kind_tag(k: crate::markdown::parse::recognition::AlertKind) -> u8 {
    use crate::markdown::parse::recognition::AlertKind::*;
    match k {
        Note => 0,
        Tip => 1,
        Important => 2,
        Warning => 3,
        Caution => 4,
    }
}

/// 一行内容对 `derived`（数学块 / 警报区域）的**相关投影签名**——纯局部判定该行的
/// region 结构是否可能变化，与 `math_regions` / `alert_regions` 的逐行判据逐条对应：
/// 围栏、`$$`、单行 `$$…$$`（**不含内部文本**——内部变化走编辑期的数学 source 修补）、
/// 警报/引述、对齐标记、行内 `$` 有无。编辑前/后签名相同 → 非结构性编辑不会改变任何
/// 区域 → `derived` Rc 跨键复用（不再全篇重扫）；不同 → 整篇重建。
pub(crate) fn region_line_sig(line: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if line.trim_start().starts_with("```") {
        h.write_u8(1);
    } else {
        let t = line.trim();
        if t == "$$" {
            h.write_u8(2);
        } else if crate::markdown::parse::inline_style::one_line_math(line).is_some() {
            h.write_u8(3);
        } else if let Some(p) = crate::markdown::parse::inline_style::blockquote_prefix(line) {
            match crate::markdown::parse::recognition::alert_prefix(&line[p..]) {
                Some((kind, _, fold)) => {
                    h.write_u8(4);
                    alert_kind_tag(kind).hash(&mut h);
                    fold.hash(&mut h);
                }
                None => h.write_u8(5),
            }
        } else if let Some(a) = crate::markdown::parse::inline_style::math_align_marker(line) {
            h.write_u8(6);
            use crate::markdown::parse::inline_style::MathAlign::*;
            match a {
                Left => h.write_u8(0),
                Center => h.write_u8(1),
                Right => h.write_u8(2),
            }
        } else {
            h.write_u8(7);
            line.as_bytes().contains(&b'$').hash(&mut h);
        }
    }
    h.finish()
}
/// 撤销/重做栈深度上限（超出丢最旧）。
pub(crate) const UNDO_LIMIT: usize = 200;
/// 后台装载结果：整篇 O(n) 解析（Doc::open + 派生标记计数）的**纯输出**。`Send`，
/// 可在 `background_executor` 任意线程构建、再回 UI 线程 [`WyEditor::install_open`]。
pub(crate) struct WyOpenResult {
    pub(crate) doc: Doc,
    pub(crate) math_dollar_cnt: usize,
    pub(crate) alert_marker_cnt: usize,
}
