use std::ops::Range;
use std::rc::Rc;
use gpui::SharedString;
use crate::markdown::doc::Doc;
use crate::markdown::wysiwyg::element;

// —— 纯渲染数据容器已随「渲染模型」迁入引擎 `markdown_wysiwyg_engine::model`（宿主
// `WyRenderModel` 的字段类型；引擎不能反向依赖宿主，故作为纯类型随迁，纯结构移动）——
// 宿主经此重导出保持既有路径（`crate::wysiwyg::types::PrebuildPass1` / `BlockMathPin` /
// `ImageSrcBlocks` / `WyDerived` 语义不变），`mod.rs` 的 `pub(crate) use self::types::{…}`
// 与 `pub use self::types::{…}` glob 链原样成立。
pub(crate) use markdown_wysiwyg_engine::model::{
    BlockMathPin, ImageSrcBlocks, InlineImageRef, PrebuildPass1, WyDerived,
};

/// 块布局的可变引用（`ensure_block_layout` 返回 / 缓存槽共用）。
/// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
#[allow(dead_code)]
pub(crate) type BlockLayoutRef = Rc<std::cell::RefCell<element::BlockLayout>>;
/// A4：标题目录条目 `(源码行, 标题级别, 标题文本)` 容器（WyEditor 目录 / 侧边栏共用）。
pub type TocEntries = Vec<(usize, u8, SharedString)>;
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
/// 行内 `![alt](src)` 的稳定标识——类型已随引擎 `WyRenderHost:hover_inline_image`/
/// `selected_inline_image` 迁入 `markdown_wysiwyg_engine::model::InlineImageRef`，上面 re-export。

/// 图片编辑卡片的目标：块级独立图片行 或 行内图片。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImageEditTarget {
    /// 独立图片行（整行 `![alt](src)`）。
    Block { src_line: usize },
    /// 行内 `![…](…)`（`InlineImageRef` 标识）。
    Inline(InlineImageRef),
}

impl ImageEditTarget {
    /// 目标图片所在源码行（块/行内统一取行）。
    pub(crate) fn src_line(&self) -> usize {
        match *self {
            Self::Block { src_line }
            | Self::Inline(InlineImageRef { src_line, .. }) => src_line,
        }
    }
}

/// 图片编辑卡片：两个单行输入框（链接 + 文本）锚定在目标图片旁。
pub(crate) struct ImageEditPopup {
    /// 目标：独立图片行 或 行内图片。
    pub(crate) target: ImageEditTarget,
    /// 图片链接（`![alt](src)` 的 src）输入框。
    pub(crate) link: gpui::Entity<ui::input::InputState>,
    /// 图片文本（alt / 题注）输入框。
    pub(crate) text: gpui::Entity<ui::input::InputState>,
}
/// 块级数学公式（`$$…$$`）的编辑卡片：**多行输入区域**（内嵌 `CodeEditor`，预填
/// 公式**正文**、不含 `$$` 定界符）锚定在目标公式卡顶部。每键即时 `apply_edit` 写回
/// 文档（撤销栈）、公式实时重渲染；长公式/多行靠 CodeEditor 自带纵横滚动不溢出；
/// 点卡片外任意处关闭（无取消语义）。标题栏兼作拖拽把手（见 [`MathEditDrag`]）。
/// 仅可编辑模式打开；只读模式双击 = no-op（保持渲染卡，不 reveal、不弹卡）。
pub(crate) struct MathEditPopup {
    /// 目标数学块的首源码行号。
    pub(crate) src_line: usize,
    /// 公式正文（不含 `$$` 定界符）的多行输入区域（内嵌 CodeEditor）。
    pub(crate) text: gpui::Entity<editor::CodeEditor>,
}
/// 数学编辑卡片**拖拽把手**进行中状态：按住标题栏（兼作把手）开始拖动。
/// `on_mouse_down` 记录抓取光标 + 抓取时缓存锚点 + 抓取前的累计偏移；窗口级
/// `on_window_mouse_move` 按 `pre_delta + (pos − grab)` 更新累计偏移并重设缓存锚点
/// （卡片跟随鼠标 1:1；paint 期刷新锚点 = `anchor_at(origin,f) + delta` 保持拖拽位移）。
#[derive(Clone, Copy)]
pub(crate) struct MathEditDrag {
    /// 抓取时指针的窗口坐标（位移起点）。
    pub(crate) grab: gpui::Point<gpui::Pixels>,
    /// 抓取时缓存的卡片锚点（本次拖拽基准；含此前累计偏移）。
    pub(crate) base_anchor: gpui::Point<gpui::Pixels>,
    /// 抓取前的累计偏移（本次位移叠加其上，使 paint 刷新与文档几何重新对齐后不丢旧位移）。
    pub(crate) pre_delta: gpui::Point<gpui::Pixels>,
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
/// 一次「内容派生」的全篇扫描结果见引擎 [`WyDerived`]（已随渲染模型迁入
/// `markdown_wysiwyg_engine::model`，宿主此名重导出；`derived()`/`fold_plan_sets` 等
/// 构建逻辑仍留宿主 view.rs）。
/// 一次「图片 src → 块下标」反向索引见引擎 [`ImageSrcBlocks`]（同上）。
/// 一段 `$$` 卡就绪位图钉见引擎 [`BlockMathPin`]（同上）。
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
/// 是否「**普通行**」（`region_line_sig` 的 class 7 分支）：非 ` ``` ` 围栏、非 `$$`
/// 围栏、非整行 `$$…$$`、非引述/警报、非对齐标记 → 该行只可能是纯文本/行内样式行。
/// M5-B3-1 子项 3 用它做增量判定：普通行 ↔ 普通行的非结构编辑（行内 `$` 有无翻转）
/// **不改变任何 region 结构**（`math_regions` 只认围栏/整行 `$$…$$`，行内 `$` 由可见带
/// 懒光栅 claim）→ 免 `regions_ver++` 整篇重扫。**必须与 `region_line_sig` 的 class-7
/// 分支同判据**（同一目录下同步维护）。
pub(crate) fn is_plain_region_line(line: &str) -> bool {
    let t = line.trim();
    !line.trim_start().starts_with("```")
        && t != "$$"
        && crate::markdown::parse::inline_style::one_line_math(line).is_none()
        && crate::markdown::parse::inline_style::blockquote_prefix(line).is_none()
        && crate::markdown::parse::inline_style::math_align_marker(line).is_none()
}
/// 撤销/重做栈深度上限（超出丢最旧）。
pub(crate) const UNDO_LIMIT: usize = 200;
/// 撤销合并时间窗：两次编辑间隔 < 300ms 才并入同一撤销单元；停顿 ≥ 该值即新起单元
/// （I1 对齐 zed `History.group_interval`，与 `editor/undo.rs::UNDO_GROUP_MS` 同位）。
pub(crate) const UNDO_GROUP_MS: std::time::Duration = std::time::Duration::from_millis(300);
/// 后台装载结果：整篇 O(n) 解析（Doc::open + 派生标记计数 + 首帧基础布局预建输入）
/// 的**纯输出**。`Send`，可在 `background_executor` 任意线程构建、再回 UI 线程
/// [`WyEditor::install_open`]。
pub struct WyOpenResult {
    pub(crate) doc: Doc,
    pub(crate) math_dollar_cnt: usize,
    pub(crate) alert_marker_cnt: usize,
    /// 首帧基础布局（Pass1）的**预建输入**（Step4-①）：后台在 `Doc::open_rope` 的
    /// 解析重叠期逐块只读自身 `BlockMeta`/行长/围栏类形态，把「每块估算高所需的全部
    /// doc 依赖量」算好并随结果带回——`install_open` 只存槽，首帧 `build_frame` 用
    /// 真实主题/宽度以 O(n) 纯算术物化成 `BlockLayout`（若干 ms），不再首帧全量
    /// doc 估高（Step1 基线 debug ~103ms / release ~29ms）。None = 同步/Str 路径
    /// （无预建，走既有 `pass1_base_layout`）。
    pub(crate) prebuild: Option<PrebuildPass1>,
}
