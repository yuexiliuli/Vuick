//! 代码折叠（视图层投影）：**绝不修改 Rope**，折叠状态是一份独立于文本的
//! 「视觉行 ↔ 逻辑行」映射视图。
//!
//! 设计约束（与编辑器固定窗口虚拟化对齐）：
//! - 折叠隐藏的是**整逻辑行**，行高保持统一 → 渲染/滚动仍跑「行数 × 行高」的
//!   像素模型，只是把「逻辑行数」换成「视觉行数」（`visual_line_count`）；
//! - 折叠区间**平坦互不相交**（v1 取舍）：映射是单调的、可二分，查询 O(log F)；
//!   嵌套折叠需要区间代数，留给后续（届时才值得上 SumTree）。
//! - `Fold { start, end }` 半开区间：`start` 行本身仍可见（折叠首行），
//!   **隐藏区间为 `(start, end)`**（`end` 行自身可见）。
//!
//! # 映射公式（F = 折叠数，全部 O(log F)）
//!
//! `hidden_prefix[i]` = 前 `i` 个折叠累计隐藏行数（`body_len = end-start-1`）。
//!
//! - `visual_row(logical)`：phys 行折叠体内部 → 塌缩到该折叠首行的视觉行；
//!   否则 `logical − 在其之前已结束的折叠隐藏行数`。
//! - `logical_row(visual)`：二分「折叠首行视觉位置」，视觉落在折叠首行 ⇒ 其
//!   逻辑行，否则 `visual + hidden_prefix`（折叠体在视觉上是「塌缩的一格」）。

// 折叠重排内核与映射类型 → 放行
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use gpui::SharedString;

#[cfg(test)]
thread_local! {
    /// 最近一次 `incremental_fold_brackets` 的实际重扫包络
    /// `(fold_from, fold_stop, bracket_from, bracket_stop)`：**确定性结构量**，供
    /// 「打字包络不退化」回归探针断言（禁纯墙钟做主断，墙钟仅旁证）；`[0,0,0,0]` =
    /// 未走增量路径（整篇兜底 / 超大文档清空）。
    static LAST_INCREMENTAL_SPANS: std::cell::Cell<[usize; 4]> =
        const { std::cell::Cell::new([0, 0, 0, 0]) };
}

/// （仅测试）最近一次 [`incremental_fold_brackets`] 的重扫包络 `(fold_from, fold_stop,
/// bracket_from, bracket_stop)`，供「栈永不清空」文档的增量包络有界回归探针读取。
#[cfg(test)]
pub(crate) fn last_incremental_spans() -> [usize; 4] {
    LAST_INCREMENTAL_SPANS.with(|c| c.get())
}

/// 一个折叠区间：隐藏 `(start, end)`，`start` 行可见、`end` 行可见。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    /// 折叠起始逻辑行（可见，渲染折叠指示器）。
    pub start: usize,
    /// 折叠结束逻辑行（**开区间**：隐藏 `[start+1, end)`，`end` 自身可见）。
    pub end: usize,
}

impl Fold {
    /// 本折叠隐藏的逻辑行数（start 之后、end 之前的行）。
    pub fn body_len(&self) -> usize {
        self.end.saturating_sub(self.start).saturating_sub(1)
    }
}

/// 一个**跨行**括号配对：括号标尺（bracket guides）的一条竖线区间，
/// 从开括号所在行**之后**一直画到闭括号所在行**之前**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketPair {
    /// 开括号所在逻辑行。
    pub open_line: usize,
    /// 开括号在行内的**字节列**（保留：诊断用）。
    pub open_col: usize,
    /// 闭括号所在逻辑行（恒 > `open_line`）。
    pub close_line: usize,
    /// 闭括号在行内的**字节列**（C4 缩进感知配对新增；供竖线列 `min(开列, 闭列, …)` 计算）。
    pub close_col: usize,
    /// 竖线所在**宽度单位列**（空格=1、Tab=4，见 [`super::buffer::char_width_units`]）：
    /// **开括号行的缩进宽**（块首行缩进，如 impl 行缩进 0、fn 行缩进 4、Self 行缩进 8）。
    /// 竖线画在这里并与块体左对齐；不画在开/闭行上（避免压到 `{`/`}` 字形）。
    pub guide_units: usize,
    /// 体内（`开+1 .. 闭−1`）最小「非闭行缩进 units」：跳过空行与「首非空白为 `} ) ]`」
    /// 的行（closer，不参与列下吸）；跨度 > 上限（1024 行，防退化）或无合格行 → `None`
    /// （= +∞，调用方退化为取 `min(开,闭)` 项）。
    /// **落地期预存**（折叠表扫描/合并时算好，**非帧态**）：帧内 `guide_pair_column*`
    /// O(1) 只读，替代运行时逐内行 `line_shared` 堆扫描（巨型括对逐帧 ×3 消费方各扫一遍
    /// 的历史成本下移到扫描/增量期只算一次）。由 [`scan_bracket_segment`] 在闭号处算、
    /// 增量「跨包络平移」处对受编辑括对重算（与整篇扫描对新增字段逐位一致，K160），
    /// 且与旧运行时算法逐字节同语义（K153 单源，parity 测试钉死）。
    /// u32 足矣：缩进 units 以行首空白计（空格=1、Tab=4），单行不可能超 2^32
    /// （行文本超 4GB 即越文档行宽上限）；`None` 兜底无需哨兵。
    pub guide_body_min_units: Option<u32>,
    /// 开括号的**嵌套层级**（0 起，纯 DFS 嵌套深度）：供括号引导线 3 色循环池按
    /// `depth % 3` 取色（对齐 VS Code bracket guides）。良构代码下 = 括号树深度；
    /// 坏代码（永不闭的 `{`、交错错配）下计数器可偏差——竖线纯视觉，偏例不影响正确性。
    /// 增量平移走 `..p` 结构更新自动保级；受影响段重扫从边界全新起算（边界各栈空 ⇒
    /// depth 0，与整篇逐位一致）。
    pub depth: usize,
}

/// 括号**字符**的嵌套层级（括号文本着色用，对齐 VS Code `bracketPairColorization`）：
/// 与 [`BracketPair`]（只记**跨行**配对、画竖线）互补——**逐字符**记录每个被词法扫描
/// 认可的括号（含单行配对、含永不匹配的开口；**成功配对**的闭括号也记、不匹配闭跳过），
/// 按 `(line, col)` 升序产出。开括号 depth = 压栈前 `nest_depth`；闭括号 depth = 对应
/// 开口的 depth（括对两端同色）。视图层按 `depth % 3` 取 `bracket_guide_palette` 给
/// 该字符 `col..col+1` 字节 span 覆前景色；字符串/注释内的括号不在扫描内 → 不着色。
/// 增量平移按 `line` 仿 [`BracketPair`] 尾部平移；受影响段重扫从边界全新起算（各栈
/// 空 ⇒ depth 0，与整篇逐位一致）。坏代码（永不闭开口、交错错配）下计数可偏差——
/// 纯视觉，偏例不影响正确性（与 `BracketPair::depth` 同口径）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketChar {
    /// 括号所在逻辑行。
    pub line: usize,
    /// 括号在行内的**字节列**（着色 span = `col..col+1`）。
    pub col: usize,
    /// 嵌套层级（0 起）：开括号 = 压栈前 `nest_depth`；闭括号 = 对应开口的 `depth`。
    pub depth: usize,
}

/// [`BracketPair`] 的轻量值拷贝（owned，`Copy`）：供「活动括对」派生与 bracket 段
/// active 匹配传值，避免持 `&BracketPair` 跨借用边界（见 `editing::active_pair`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BracketPairLite {
    /// 见 [`BracketPair::open_line`]。
    pub open_line: usize,
    /// 见 [`BracketPair::open_col`]。
    pub open_col: usize,
    /// 见 [`BracketPair::close_line`]。
    pub close_line: usize,
    /// 见 [`BracketPair::close_col`]。
    pub close_col: usize,
    /// 见 [`BracketPair::depth`]。
    pub depth: usize,
    /// 见 [`BracketPair::guide_body_min_units`]。
    pub guide_body_min_units: Option<u32>,
}

impl From<&BracketPair> for BracketPairLite {
    fn from(p: &BracketPair) -> Self {
        Self {
            open_line: p.open_line,
            open_col: p.open_col,
            close_line: p.close_line,
            close_col: p.close_col,
            depth: p.depth,
            guide_body_min_units: p.guide_body_min_units,
        }
    }
}

/// 行首空白（空格/Tab）的计数结果：**一次扫描**同时给出字节数与宽度单位两种口径，
/// 核心/视图两层共用这一处实现，杜绝跨 crate 双实现漂移。
///
/// - [`LeadingWs::bytes`]：行首空白**字节数**（空格/Tab 各 1）——折叠候选缩进比较、
///   缩进折叠启发式用（与历史 `view::leading_ws_len` / `leading_ws_bytes` 逐位一致）；
/// - [`LeadingWs::units`]：**宽度单位数**（空格=1、Tab=4，与 `char_width_units` 一致）——
///   括号标尺竖线列用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadingWs {
    spaces: usize,
    tabs: usize,
}

impl LeadingWs {
    /// 扫描 `s` 行首连续空格/Tab（遇其它字节即停；空行 → 全 0）。
    pub fn scan(s: &str) -> Self {
        let mut spaces = 0;
        let mut tabs = 0;
        for b in s.bytes() {
            match b {
                b' ' => spaces += 1,
                b'\t' => tabs += 1,
                _ => break,
            }
        }
        Self { spaces, tabs }
    }

    /// 行首空白**字节数**（空格/Tab 各 1）。
    pub fn bytes(self) -> usize {
        self.spaces + self.tabs
    }

    /// 行首空白**宽度单位数**（空格=1、Tab=4）。
    pub fn units(self) -> usize {
        self.spaces + self.tabs * 4
    }
}

/// 行首空白（空格/Tab）的统一入口：`leading_indent(s).bytes()` / `leading_indent(s).units()`。
pub fn leading_indent(s: &str) -> LeadingWs {
    LeadingWs::scan(s)
}

// ---------------------------------------------------------------------------
// §8 缩进引导线（VS Code indentation guides 主源）纯函数
// ---------------------------------------------------------------------------

/// 缩进引导线级数（§8.1 主源算法）：`level = (indent=0) ? 0 : ceil(cols/is)`。
/// `cols` = 行首空白的**宽度单位**（空格=1、Tab=4，即 [`leading_indent`].units()）；
/// `is` = indentSize（本仓默认 `config::layout::INDENT_GUIDE_INDENT_SIZE=4`）。
/// 纯函数（无 gpui 依赖），editor_core 与视图层共用一处实现。
/// 画列 = `{(L−1)·is | L=1..=level}`（0-based 列 g=0,is,2is,…）。
pub fn indent_guide_level(leading_cols: usize, indent_size: usize) -> u32 {
    let (d, r) = (leading_cols / indent_size, leading_cols % indent_size);
    if r == 0 {
        d as u32
    } else {
        (d + 1) as u32
    }
}

/// 空/纯空白行的引导线级数（§8.1）：A=上一可视**非空**行缩进、B=下一可视**非空**行缩进
/// （任一缺 → 0，monaco 同：首行/文档尾空行无线）。四分支：
/// - `A<B` → `1 + ⌊A/is⌋`（从浅层 A 续一段警戒线）；
/// - `A==B` → `⌈B/is⌉`（均等缩进 → 按本级续）；
/// - `A>B` → `1 + ⌊B/is⌋`（从浅层 B 续，防竖线直穿闭合行）。
///
/// cols 传 `Option<i32>`（None = 缺）；返回应画的级数（0 = 无线）。
pub fn blank_guide_level(prev: Option<i32>, next: Option<i32>, indent_size: i32) -> u32 {
    let is = indent_size.max(1);
    let (Some(a), Some(b)) = (prev, next) else {
        return 0;
    };
    let a = a.max(0);
    let b = b.max(0);
    if a < b {
        1 + (a / is) as u32
    } else if a == b {
        ((b + is - 1) / is) as u32
    } else {
        1 + (b / is) as u32
    }
}

/// 跨行括号配对的**段扫描**结果。
#[derive(Debug, Default)]
pub(crate) struct BracketScanReport {
    /// 段内产生的配对（`open_line ∈ [from, stop)`；`close_line ≤ stop`）。
    pub pairs: Vec<BracketPair>,
    /// 段内产生的括号**字符**层级表（按 `(line, col)` 升序；含单行配对与永不匹配的
    /// 开口、成功配对的闭括号，见 [`BracketChar`]）。
    pub chars: Vec<BracketChar>,
    /// 段内「栈空 + 不在块注释内」边界行（升序；含 `from`）。
    pub boundaries: Vec<usize>,
    /// 实际扫描停止行（`stop_after` 命中首个边界后的行；未命中 = `row_count`）。
    pub stop: usize,
    /// 段内「对齐行」记录的**入口栈状态快照**（升序；行 → 处理前行状态，见
    /// [`SNAPSHOT_SPACING`] / ⑦ 打字卡顿修复）。空栈边界照旧，快照是「栈永不清空」
    /// 文档的替代断点；空表 = 无对齐落点（短段）或预算封顶（记忆有界）。
    pub snapshots: Vec<(usize, BracketSnapshot)>,
    /// 扫描预算耗尽（病理文档：单行密括号 / 超深缩进）→ true：输出可能不全，上层应
    /// **整体返回空表**（宁缺毋残，避免「有开无闭」的残缺高亮），见 `BRACKET_*_MAX`。
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// 括号扫描预算（2026-11-14 安全护栏）：扫描本无上限，病理文档会无界增长三处内存直至
// OOM——① 单行密括号（1 行 2.5×10^7 个 `{`/`(`/`[`，row_count=1 绕开 limits.rs 200k
// 行守卫）：out.chars 24B/项 + paren/brace 栈 32/24B/项同步爆；② 超深缩进（单行
// `" ".repeat(10^7) + "{"`）：brace_stack.resize(k+1) 按缩进 depth 膨胀空层。
// 预算耗尽 → truncated 置位、停止一切后续追加（`}` pop 仍执行防空栈 panic），上层见
// truncated → 返回空表（该文件整体不渲染括对/高亮，视觉降级可接受）。数值取 2 的幂，
// 正常源码（10 万行 × 平均每行 3~5 个括字符 ≈ 3~5×10^5）约为上限（1<<20 ≈ 1.05×10^6）的
// 1/2~1/3，触发即属病理输入或超大生成文件；即便整个预算吃满：chars 表 ≈24MB、未闭栈 ≈32MB、
// 派生 pair 表 ≈24MB、快照克隆最坏 ~100MB 级暂态，仍是有界而非 OOM。
// ---------------------------------------------------------------------------
/// 总括字符表（`out.chars`）的 push 上限：1<<20 ≈ 104.9 万项 ≈ 24B/项 = 24MB（有界上界）。
pub(crate) const BRACKET_CHAR_MAX: usize = 1 << 20;
/// 未闭栈项上限（paren_stack + Σ brace 各层）：同量级 ≈ 32MB（32B/项），与字符上限
/// 联动约束「只开不闭」型病理输入的栈无界增长。
pub(crate) const BRACKET_UNCLOSED_MAX: usize = 1 << 20;
/// 缩进分层栈层数上限：`brace_stack` 只 resize 到此深度，防御超深缩进的空 Vec 膨胀
/// （正常代码缩进深度远小于 4096；视觉上也不存在 4096 层竖线）。
pub(crate) const BRACKET_DEPTH_MAX: usize = 4096;

/// 体内（`开+1 .. 闭−1`）最小「非闭行缩进 units」**落地区间扫描**（语义 = 旧视图层运行时
/// `guide_inner_min_noncloser` 逐字节复刻，K153/K160 单源）：跳过空行与「首非空白为
/// `} ) ]`」的行（closer，不参与列下吸）；跨度 > `INNER_SCAN_CAP`（1024 行，防退化）
/// 或无合格行 → `None`（= +∞，调用方退化为取 `min(开,闭)` 项）。
/// 返回 u32（缩进 units，见 [`crate::editor::buffer::char_width_units`]）。
///
/// 用途：**落地期预存**（[`scan_bracket_segment`] 闭号处 / 增量「跨包络平移」处重算），
/// 帧内 `guide_pair_column*` 只读字段、不再逐内行 `line_shared` 堆扫。全扫描期逐对
/// 内扫有界：单对 ≤ 1024 行读（跨度超限即返回 None、零读）；病理深嵌套文档（栈永不
/// 清空型）的对数 × 每对体长在预算内，且运行在后台线程、非帧热路径（代价转移是目标）。
fn compute_guide_body_min_units<F>(line: &F, open: usize, close: usize) -> Option<u32>
where
    F: Fn(usize) -> SharedString,
{
    const INNER_SCAN_CAP: usize = 1024;
    if close.saturating_sub(open).saturating_sub(1) > INNER_SCAN_CAP {
        return None;
    }
    let mut min: Option<u32> = None;
    for r in (open + 1)..close {
        let s = line(r);
        let s = s.as_ref();
        let t = s.trim_start();
        if t.is_empty() {
            continue;
        }
        let first = t.as_bytes()[0];
        if first == b'}' || first == b')' || first == b']' {
            continue;
        }
        let units = leading_indent(s).units() as u32;
        min = Some(min.map_or(units, |m| m.min(units)));
    }
    min
}

// ---------------------------------------------------------------------------
// 增量重扫「栈永不清空」文档的快照断点（2026-11-14 打字卡顿 ⑦ 修复）：
// 空栈边界依赖「整篇栈清空」才出现；深嵌套/未闭括号文档（单 `{` 未闭就令缩进层级栈
// 永不清空）会令边界列表稀疏/空 → 受影响包络退化为 0..EOF，每次敲括号整篇重扫
// （20k 深嵌套 debug 85-135ms）。对策 = 在**对齐行**持久化「入口栈状态」快照：
// 增量重扫从最近快照还原真态（无需等栈空），并在「新态 == 旧态」的快照行汇聚提前
// 收敛，与从 0 整篇扫描逐位一致（K160 对拍唯一真值，不用正确性换速度）。
// 内存有界：快照点 = 行数/对齐间距、态条目预算按 `BRACKET_UNCLOSED_MAX` 同量级护栏
// 封顶（超限不再落新快照，该行退化非快照点、由旧的空栈边界/0 兜底），不会无界增长。
// ---------------------------------------------------------------------------
/// 快照对齐间距：行号对齐到该倍数的行才落快照（fold/括号各一族）。正常文档的空栈
/// 边界已足够密；此间距仅覆盖「栈永不清空」病理文档——越密包络越小、内存越多。
pub(crate) const SNAPSHOT_SPACING: usize = 512;
/// 单族（fold / 括号各算一族）快照**条目总预算**：全部快照栈项合计 ≤ 此值
/// （每项约 16~24B → 预算 ≈ 1~1.5MB 有界上界）；超限不再落新快照。
pub(crate) const SNAPSHOT_STATE_ITEMS_BUDGET: usize = 1 << 16;

/// fold 扫描器入口状态快照（某行「处理前」的完整状态）：重扫从它还原即与从 0 整篇
/// 扫描在 `≥ 该行` 逐位一致；`PartialEq` 供增量汇聚点「新态 == 旧态」校验（K160）。
/// 跨 crate 由视图层**不透明**存储/透传（不进字段访问）。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct FoldSnapshot {
    /// 挂起的块 `(start, indent)` 栈（见 [`scan_fold_segment`]）。
    pub stack: Vec<(usize, usize)>,
    /// 上一非空行 `(line, indent)`。
    pub pending: Option<(usize, usize)>,
}

/// 括号扫描器入口状态快照（某行「处理前」的完整状态）：重扫从它还原即与从 0 整篇
/// 扫描在 `≥ 该行` 逐位一致，含 C4 缩进分层花括号栈 / 全局括号栈 / 块注释位 / 嵌套
/// 深度。`PartialEq` 用于增量汇聚点「新态 == 旧态」校验（字节恒等由状态相等 + 行内容
/// 一致保证，K160）；跨 crate 由视图层**不透明**存储/透传。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[doc(hidden)]
pub struct BracketSnapshot {
    /// 各缩进层的花括号 LIFO 栈 `(开行, 开列, depth)`。
    pub brace_stack: Vec<Vec<(usize, usize, usize)>>,
    /// 全局圆括号/方括号 LIFO 栈 `(开行, 开列, 字符, depth)`。
    pub paren_stack: Vec<(usize, usize, char, usize)>,
    /// 是否正处块注释内。
    pub in_block_comment: bool,
    /// 纯 DFS 嵌套深度。
    pub nest_depth: usize,
}

/// 扫描全文/受影响段的跨行括号配对（`{ }` / `( )` / `[ ]`），O(行数) 单遍、仅记录
/// **跨行**配对（单行 `{}` 不产竖线）。`line` 按逻辑行号返回该行文本（只读）。
///
/// 配对规则（C4 缩进感知）：`{` 压入「开行缩进 units 层」的独立栈（`}`/`(`/`[` 不混层）；
/// `}` 只查**本行缩进层**栈、命中最近未配 `{` 成对；该层空 → **丢弃**（绝不跨层偷配，
/// 消灭坏代码里 mega 单行多括号吞掉别的层的闭号导致的伪标尺）。`()`/`[]` 保留全局 LIFO。
///
/// 词法兜底：跳过 `//` 行注释、`/* */` 块注释（跨行状态）与 `"..."` 字符串
/// （含 `\"` 转义）；**不**处理原样字符串/字符字面量（Rust 生命周期 `'a` 不宜当
/// 字符字面量跳过，否则会吞掉整行 → 可选本忽略少数偏例）。
/// 配平失败（交错/缺失）时丢弃不平衡项——竖线是纯视觉参考，偏例不影响正确性。
///
/// `scan_bracket_pairs` 是其「从 0 整篇、扫到文末」的薄包装；[`BracketScanReport`]
/// 额外给出「栈清空边界」供增量重算复用：
/// **从某个边界行以全新状态开始（各层花括号栈空 && 括号栈空 && 不在块注释内），
/// 与从 0 整篇扫描在 `≥ 该行` 的处理上逐位一致**——括号不跨行（字符串在行内结束）、
/// 进入行只需栈 + 块注释位两个状态，边界后无任何 OPEN 段残留，故可断点续扫。
/// `stop_after = Some(m)` 时在**首个「行号 ≥ m 且处理后为边界」的行**停止
/// （增量重算用它圈定受影响包络尾；未命中则扫到文末）。
///
/// 2026-11-14（打字卡顿 ⑦）：另支持两种「栈永不清空」文档的断点——
/// - `init = Some(快照)`：以该快照（某行处理前状态）为起始态续扫（与从 0 整篇在
///   `≥ from` 逐位一致，无需等栈空）；
/// - `merge_snaps = &[(行, 期待旧态)]`：扫描到某检查点行时，若当前**入口态** == 期待
///   旧态（即受影响段对栈的影响已在此行完全消解）→ 处理完该行后在此停（`out.stop`），
///   尾部平移与整篇扫描等价（K160）。检查点行须 ≥ 受影响段尾（由增量侧预筛）。
pub(crate) fn scan_bracket_segment<F>(
    line: F,
    row_count: usize,
    from: usize,
    stop_after: Option<usize>,
    init: Option<BracketSnapshot>,
    merge_snaps: &[(usize, &BracketSnapshot)],
) -> BracketScanReport
where
    F: Fn(usize) -> SharedString,
{
    let mut out = BracketScanReport::default();
    if row_count == 0 {
        return out;
    }
    let from = from.min(row_count);
    // C4 缩进感知配对：
    // - 花括号按「开行缩进 units」分层，每层独立 LIFO；`}` 只查**本行**缩进层栈，
    //   命中最近未配 `{`；该层空 → 丢弃（绝不跨层偷配）。栈项 = (开括号行, 开括号字节列, depth)。
    // - 圆括号/方括号保留全局 LIFO（不变）。栈项 = (开括号行, 开括号字节列, 字符, depth)。
    // - `nest_depth` = 与分层栈解耦的**纯 DFS 嵌套深度**：每个开括号压入时记录当前
    //   nest_depth 后 +1；闭括号**成功配对弹出**时 −1（坏代码下可偏差，见 `BracketPair::depth`）。
    let mut brace_stack: Vec<Vec<(usize, usize, usize)>> = Vec::new();
    let mut paren_stack: Vec<(usize, usize, char, usize)> = Vec::new();
    let mut nest_depth = 0usize;
    let mut in_block_comment = false;
    // 起始态：空栈（from=0 / 空栈边界）或由快照还原（「栈永不清空」文档断点，含残差开口）。
    if let Some(s) = init {
        brace_stack = s.brace_stack;
        paren_stack = s.paren_stack;
        in_block_comment = s.in_block_comment;
        nest_depth = s.nest_depth;
    }
    // 预算护栏状态：`truncated` = 预算耗尽（停止一切追加、返回截断报告）；
    // `unclosed` = 未闭栈项数（paren_stack.len() + Σ brace 各层 len，压栈 +1 / 弹出 −1），
    // 与字符上限联动约束「只开不闭」型病理输入的栈无界增长。
    let mut truncated = false;
    let mut unclosed = paren_stack.len();
    for level in &brace_stack {
        unclosed += level.len();
    }
    // 快照落点预算（⑦）：全部快照栈项合计 ≤ `SNAPSHOT_STATE_ITEMS_BUDGET`（有界上界，
    // 超限不再落新快照，该行退化非快照点）；`merge_cur` 为汇聚检查点游标。
    let mut snap_items_budget = SNAPSHOT_STATE_ITEMS_BUDGET;
    let mut merge_cur = 0usize;
    let mut line_ix = from;
    while line_ix < row_count {
        // ——— 处理本行之前：快照落点 + 汇聚检查点（都看「入口态」）———
        let snap_items = paren_stack.len() + brace_stack.iter().map(|lv| lv.len()).sum::<usize>();
        if line_ix.is_multiple_of(SNAPSHOT_SPACING) && snap_items <= snap_items_budget {
            out.snapshots.push((
                line_ix,
                BracketSnapshot {
                    brace_stack: brace_stack.clone(),
                    paren_stack: paren_stack.clone(),
                    in_block_comment,
                    nest_depth,
                },
            ));
            snap_items_budget -= snap_items;
        }
        let mut snap_stop = false;
        if merge_cur < merge_snaps.len() && merge_snaps[merge_cur].0 == line_ix {
            let exp = &merge_snaps[merge_cur].1;
            if exp.brace_stack == brace_stack
                && exp.paren_stack == paren_stack
                && exp.in_block_comment == in_block_comment
                && exp.nest_depth == nest_depth
            {
                // 受影响段对栈的影响已在本行完全消解 → 处理完本行后在此停（尾部平移等价）。
                snap_stop = true;
            }
            merge_cur += 1;
        }
        let text: SharedString = line(line_ix);
        let s = text.as_ref();
        let mut col = 0usize;
        while col < s.len() {
            if truncated {
                break; // 预算耗尽：本行剩余不再扫描，返回截断报告
            }
            let ch = s[col..].chars().next().unwrap_or('\0');
            if in_block_comment {
                if s[col..].starts_with("*/") {
                    in_block_comment = false;
                    col += 2;
                } else {
                    col += ch.len_utf8();
                }
                continue;
            }
            match ch {
                '/' if s[col..].starts_with("//") => break,
                '/' if s[col..].starts_with("/*") => {
                    in_block_comment = true;
                    col += 2;
                }
                '"' => {
                    // 跳过字符串字面量（`\"` 转义）。
                    col += 1;
                    while col < s.len() {
                        let c = s[col..].chars().next().unwrap_or('\0');
                        col += c.len_utf8();
                        if c == '\\' && col < s.len() {
                            let e = s[col..].chars().next().unwrap_or('\0');
                            col += e.len_utf8();
                        } else if c == '"' {
                            break;
                        }
                    }
                }
                '{' => {
                    if out.chars.len() >= BRACKET_CHAR_MAX || unclosed >= BRACKET_UNCLOSED_MAX {
                        // 字符/未闭预算耗尽 → 截断（停止一切后续追加）。
                        truncated = true;
                    } else {
                        let k = leading_indent(s).units();
                        if k >= BRACKET_DEPTH_MAX {
                            // 超深缩进：resize 空层会随缩进 depth 无界膨胀 → 截断。
                            truncated = true;
                        } else {
                            if brace_stack.len() <= k {
                                brace_stack.resize(k + 1, Vec::new());
                            }
                            out.chars.push(BracketChar { line: line_ix, col, depth: nest_depth });
                            brace_stack[k].push((line_ix, col, nest_depth));
                            unclosed += 1;
                            nest_depth += 1;
                        }
                    }
                    col += ch.len_utf8();
                }
                '(' | '[' => {
                    if out.chars.len() >= BRACKET_CHAR_MAX || unclosed >= BRACKET_UNCLOSED_MAX {
                        // 字符/未闭预算耗尽 → 截断（停止一切后续追加）。
                        truncated = true;
                    } else {
                        out.chars.push(BracketChar { line: line_ix, col, depth: nest_depth });
                        paren_stack.push((line_ix, col, ch, nest_depth));
                        unclosed += 1;
                        nest_depth += 1;
                    }
                    col += ch.len_utf8();
                }
                '}' | ')' | ']' => {
                    if ch == '}' {
                        // `}` 只查本行缩进层栈；该层空 → 丢弃（不跨层回退、不跨层偷配）。
                        let k = leading_indent(s).units();
                        if let Some(level) = brace_stack.get_mut(k)
                            && let Some((ol, oc, od)) = level.pop() {
                                // pop 恒执行（防空栈/budget 会计），追加仅在未截断时。
                                unclosed = unclosed.saturating_sub(1);
                                if !truncated {
                                    out.chars.push(BracketChar { line: line_ix, col, depth: od });
                                    nest_depth = nest_depth.saturating_sub(1);
                                    if line_ix > ol {
                                        // 竖线列 = 开括号行的缩进宽（块首缩进，兼容字段）。
                                        let guide_units = leading_indent(line(ol).as_ref()).units();
                                        out.pairs.push(BracketPair {
                                            open_line: ol,
                                            open_col: oc,
                                            close_line: line_ix,
                                            close_col: col,
                                            guide_units,
                                            guide_body_min_units: compute_guide_body_min_units(&line, ol, line_ix),
                                            depth: od,
                                        });
                                    }
                                }
                            }
                    } else {
                        let want = if ch == ')' { '(' } else { '[' };
                        while let Some((ol, oc, och, od)) = paren_stack.pop() {
                            unclosed = unclosed.saturating_sub(1);
                            if och == want {
                                if !truncated {
                                    out.chars.push(BracketChar { line: line_ix, col, depth: od });
                                    nest_depth = nest_depth.saturating_sub(1);
                                    if line_ix > ol {
                                        let guide_units = leading_indent(line(ol).as_ref()).units();
                                        out.pairs.push(BracketPair {
                                            open_line: ol,
                                            open_col: oc,
                                            close_line: line_ix,
                                            close_col: col,
                                            guide_units,
                                            guide_body_min_units: compute_guide_body_min_units(&line, ol, line_ix),
                                            depth: od,
                                        });
                                    }
                                }
                                break;
                            }
                        }
                    }
                    col += ch.len_utf8();
                }
                _ => {
                    col += ch.len_utf8();
                }
            }
        }
        if truncated {
            // 预算耗尽 → 截断报告（上层据此整体返回空表）。
            out.stop = line_ix;
            out.truncated = true;
            return out;
        }
        if snap_stop {
            // 汇聚检查点命中：入口态 == 旧快照态 → 本行产出照常落入，尾部平移等价
            // （K160）。快照行不一定是空栈边界，不必 push boundaries。
            out.stop = line_ix;
            return out;
        }
        let stack_empty = paren_stack.is_empty()
            && brace_stack.iter().all(|level| level.is_empty());
        if stack_empty && !in_block_comment {
            // 各层花括号栈 + 括号栈全空 + 不在块注释内 = 安全断点（C4 断点语义：
            // 从该行以全新状态续扫与整篇扫描在 `≥ 该行` 逐位一致）。
            out.boundaries.push(line_ix);
            if let Some(m) = stop_after
                && line_ix >= m {
                    out.stop = line_ix;
                    return out;
                }
        }
        line_ix += 1;
    }
    out.stop = row_count;
    out.truncated = truncated;
    out
}

/// 扫描全文的跨行括号配对（薄包装；语义与历史逐位一致）。
pub fn scan_bracket_pairs<F>(line: F, row_count: usize) -> Vec<BracketPair>
where
    F: Fn(usize) -> SharedString,
{
    let r = scan_bracket_segment(line, row_count, 0, None, None, &[]);
    // 预算耗尽（安全护栏）→ 空表（消费者空表 → 无竖线/无着色，宁缺毋残）。
    if r.truncated {
        Vec::new()
    } else {
        r.pairs
    }
}

/// 折叠候选的**段扫描**结果。
#[derive(Debug, Default)]
pub struct FoldScanReport {
    /// 段内产生的折叠候选（`start ∈ [from, stop)`；`end ≤ stop`）。
    pub folds: Vec<Fold>,
    /// 段内「栈清空边界」行（升序；含 `from`——从边界以全新状态继续扫描与
    /// 从 0 整篇扫描在 `≥ from` 的行上逐位一致，见 [`scan_fold_segment`]）。
    pub boundaries: Vec<usize>,
    /// 实际扫描停止行（`stop_after` 命中首个边界后的行；未命中 = `row_count`）。
    pub stop: usize,
    /// 段内「对齐行」记录的**入口栈状态快照**（升序；行 → 处理前行状态，见
    /// [`SNAPSHOT_SPACING`] / ⑦ 打字卡顿修复），语义同 [`BracketScanReport::snapshots`]。
    pub snapshots: Vec<(usize, FoldSnapshot)>,
}

/// 扫描全文/受影响段的「可折叠候选」（缩进 + 闭记号启发式，与视图层解耦，可在
/// 后台线程/只读 Rope 快照上运行，语义等价于原 `CodeEditor::scan_fold_candidates`）。
///
/// 行 `i` 是候选起点 ⟺ 其后的首个非空行缩进更深（该行将藏起一个块）；块结束 = 首个
/// 缩进回退 `/` 同级闭记号（`}` 等）行。`line` 按逻辑行号返回该行文本（只读）。
/// 缩进口径 = 行首空白**字节数**（[`LeadingWs::bytes`]，与折叠缩进启发式一致）。
///
/// `scan_fold_candidates` 是其「从 0 整篇、扫到文末」的薄包装。
/// 折叠扫描的持久状态只有 `stack`（`pending` 恒可由「上一非空行」行文本重建），
/// 故**栈空行是安全断点**：一条非空边界行 r 上，上一非空行 p 不可能被压栈
/// （压栈需 `indent_r > indent_p`，而 r 是边界意味着处理后栈空）、pending 进入
/// 下一行也仅由 r 决定 → 从 r 以全新状态开始与从 0 整篇扫描在 `≥ r` 逐位一致。
/// `stop_after = Some(m)` 时在**首个「行号 ≥ m 且处理后栈空」的行**停止
/// （增量重算用它圈定受影响包络尾；未命中则扫到文末）。
///
/// 2026-11-14（打字卡顿 ⑦）：另支持两种「栈永不清空」文档的断点——`init = Some(快照)`
/// 以快照（某行处理前状态）为起始态续扫、`merge_snaps` 为「入口态 == 期待旧态」即停的
/// 汇聚检查点（语义同 [`scan_bracket_segment`]）。
pub(crate) fn scan_fold_segment<F>(
    line: F,
    row_count: usize,
    from: usize,
    stop_after: Option<usize>,
    init: Option<FoldSnapshot>,
    merge_snaps: &[(usize, &FoldSnapshot)],
) -> FoldScanReport
where
    F: Fn(usize) -> SharedString,
{
    let mut out = FoldScanReport::default();
    if row_count == 0 {
        return out;
    }
    let from = from.min(row_count);
    // 栈：挂起的块 `(start, indent)`。pending 记录上一非空行；下次看到更深行才入栈。
    let (mut stack, mut pending) = match init {
        Some(s) => (s.stack, s.pending),
        None => (Vec::new(), None),
    };
    // 快照落点预算（⑦）：全部快照栈项合计 ≤ `SNAPSHOT_STATE_ITEMS_BUDGET`（有界上界）。
    let mut snap_items_budget = SNAPSHOT_STATE_ITEMS_BUDGET;
    let mut merge_cur = 0usize;
    let mut r = from;
    while r < row_count {
        // ——— 处理本行之前：快照落点 + 汇聚检查点（都看「入口态」；含空行——下次非空
        // 行由该态决定）———
        let snap_items = stack.len() + usize::from(pending.is_some());
        if r.is_multiple_of(SNAPSHOT_SPACING) && snap_items <= snap_items_budget {
            out.snapshots.push((r, FoldSnapshot { stack: stack.clone(), pending }));
            snap_items_budget -= snap_items;
        }
        let mut snap_stop = false;
        if merge_cur < merge_snaps.len() && merge_snaps[merge_cur].0 == r {
            let exp = &merge_snaps[merge_cur].1;
            if exp.stack == stack && exp.pending == pending {
                // 受影响段对折叠栈的影响已在本行完全消解 → 处理完本行后在此停。
                snap_stop = true;
            }
            merge_cur += 1;
        }
        let text = line(r);
        let trimmed = text.trim_start();
        if !trimmed.is_empty() {
            let indent = leading_indent(text.as_ref()).bytes();
            let closing = matches!(
                trimmed.chars().next(),
                Some('}') | Some(')') | Some(']') | Some('#') | Some('`')
            );
            if let Some((ps, pi)) = pending {
                // 上一行是块起点 ⟺ 当前行缩进更深。
                if indent > pi {
                    stack.push((ps, pi));
                }
                // 关闭缩进 ≥ 当前行的块（缩进回退或同级闭记号）。
                while let Some(&(bs, bi)) = stack.last() {
                    if indent < bi || (indent == bi && closing) {
                        if r >= bs + 2 {
                            out.folds.push(Fold { start: bs, end: r });
                        }
                        stack.pop();
                        continue;
                    }
                    break;
                }
            }
            pending = Some((r, indent));
            if stack.is_empty() {
                // 栈清空 = 安全断点（此后从空栈继续与整篇扫描逐位一致）。
                out.boundaries.push(r);
                if let Some(m) = stop_after
                    && r >= m {
                        out.stop = r;
                        return out;
                    }
            }
        }
        if snap_stop {
            // 汇聚检查点命中（含空行断点）：入口态 == 旧快照态 → 本行产出照常落入、
            // 尾部平移与整篇等价（K160）。快照行不一定是空栈边界，不必 push boundaries。
            out.stop = r;
            return out;
        }
        r += 1;
    }
    // EOF：关闭仍开着的块（延伸到最后一行）。
    let end = row_count;
    while let Some((bs, _bi)) = stack.pop() {
        if end >= bs + 2 {
            out.folds.push(Fold { start: bs, end });
        }
    }
    out.stop = row_count;
    out
}

/// 全篇扫描「可折叠候选」**纯函数**（薄包装；语义与历史逐位一致）。
/// 可见性：`editor`（视图层）的 `tests/view.rs` 跨 crate 引用，`doc(hidden) pub`。
#[doc(hidden)]
pub fn scan_fold_candidates<F>(line: F, row_count: usize) -> Vec<Fold>
where
    F: Fn(usize) -> SharedString,
{
    scan_fold_segment(line, row_count, 0, None, None, &[]).folds
}

/// 整篇扫描折叠候选 + 括号配对 + 括号字符 + 各自「栈清空边界」+ 各自**入口栈快照网格**
/// （打开/整篇替换路径用，一次性把七份结果全部落地；快照见 [`SNAPSHOT_SPACING`] /
/// ⑦ 打字卡顿修复，供后续编辑的增量起点/汇聚复用）。
/// 可见性：`editor`（视图层）的 `editing.rs` 跨 crate 消费 → 转 pub。
pub fn full_fold_brackets<F>(
    line: F,
    row_count: usize,
) -> (
    Vec<Fold>,
    Vec<BracketPair>,
    Vec<BracketChar>,
    Vec<usize>,
    Vec<usize>,
    Vec<(usize, FoldSnapshot)>,
    Vec<(usize, BracketSnapshot)>,
)
where
    F: Fn(usize) -> SharedString + Copy,
{
    let fr = scan_fold_segment(line, row_count, 0, None, None, &[]);
    let br = scan_bracket_segment(line, row_count, 0, None, None, &[]);
    // 括号扫描预算耗尽（安全护栏）→ 括对/字符/括对边界整体返回空表（消费者 partition_point
    // 空表 → None → 该文件不渲染括对/高亮，宁缺毋残）；折叠候选（缩进启发式、有界）不受影响。
    if br.truncated {
        return (
            fr.folds,
            Vec::new(),
            Vec::new(),
            fr.boundaries,
            Vec::new(),
            fr.snapshots,
            Vec::new(),
        );
    }
    (
        fr.folds,
        br.pairs,
        br.chars,
        fr.boundaries,
        br.boundaries,
        fr.snapshots,
        br.snapshots,
    )
}

/// 单个受影响行段的**增量重算**（整篇扫描的等价替代，纯函数、可在后台线程跑，
/// 对拍契约：对同一文档同一次编辑，输出与整篇扫描**逐位一致**）。
///
/// 前提（调用方保证，见本仓库 view 层 `scan_pending_edit` / `scan_multi_edit`）：
/// - `prev_folds` / `prev_pairs` / `prev_chars` / `prev_fb` / `prev_pb` 是**上次落地**
///   的扫描结果，对应「编辑前」文档；`prev_fsnaps` / `prev_bsnaps` 是同次落地的
///   「入口栈快照网格」（⑦ 打字卡顿修复，栈永不清空文档的断点）；
/// - 自上落地后只累积了**一个**受影响行段 `edit = Some((line_a, min_stop, delta))`：
///   `line_a` = 编辑首行（编辑前坐标），`min_stop` = 新坐标下「首个未受影响行」的
///   边界下限，`delta` = 净行增量（新行数 − 旧行数）；
/// - 多个编辑段累积或前提缺失 → 调用方置 `multi = true` / `edit = None` →
///   本函数退化为整篇扫描（正确性兜底）。
///
/// 机制（fold 候选与括号配对各自独立）：
/// 1. `B` = `≤ line_a` 中「栈清空边界」与「入口栈快照」较靠后的一者（[`pick_start`]）；
///    快照起点还原真态重扫、无需等栈空 → 根治「栈永不清空」文档的包络退化（⑦）；
/// 2. 在「受影响包络 `[B, stop)`」上局部重扫：`stop` = 首个 `≥ min_stop` 的**空栈边界**
///
///   或首个「新态 == 旧态」的**快照汇聚点**（⑦：栈影响已消解 → 尾部平移与整篇等价），
///    未命中则扫到文末；
/// 3. 合并（快照起点下存在「开在 B 前、闭/end 在包络内或尾段」的跨包络项，由
///    「闭号/end > stop_old → 平移保留、否则重扫重发」规则精确兜住 → 与整篇逐位一致，
///    K160）：`start < B` 的旧项保留、`start ∈ [B, stop_old)` 的旧项被重扫结果替换、
///    `start ≥ stop_old`（`stop_old = stop − delta`）的旧项按 `delta` **平移**
///    （未变段尾部行号平移）；边界/快照网格同款合并（尾部平移）。括号字符 `chars` 按
///    `line < bb` 原样保留、`[bb, stop_old_b)` 替换、`line ≥ stop_old_b` 尾部 `+delta`。
///
/// 返回 `(fold_candidates, bracket_pairs, bracket_chars, fold_boundaries,
/// bracket_boundaries, fold_snapshots, bracket_snapshots)`，与整篇扫描等价
/// （调用方可再按 `start` / `open_line` / `line` 排序）。
///
/// 从「栈清空边界」与「入口栈快照」各取最近 `≤ line_a` 者，返回更靠后的那一个及其
/// 是否为快照（并列取快照——入口态含残差开口、可重扫被编辑行，严格不弱于空栈边界；
/// 两者皆无 → `(0, false)`，从 0 整段重扫）。
fn pick_start<L>(
    boundaries: &[usize],
    snaps: &[(usize, L)],
    line_a: usize,
) -> (usize, bool) {
    let b = boundaries.partition_point(|&b| b <= line_a).checked_sub(1);
    let s = snaps.partition_point(|(l, _)| *l <= line_a).checked_sub(1);
    match (b, s) {
        (Some(bi), Some(si)) if snaps[si].0 >= boundaries[bi] => (snaps[si].0, true),
        (Some(bi), _) => (boundaries[bi], false),
        (None, Some(si)) => (snaps[si].0, true),
        (None, None) => (0, false),
    }
}

pub(crate) fn incremental_fold_brackets<F>(
    line: F,
    row_count: usize,
    full_limit: usize,
    prev_folds: &[Fold],
    prev_pairs: &[BracketPair],
    prev_chars: &[BracketChar],
    prev_fb: &[usize],
    prev_pb: &[usize],
    prev_fsnaps: &[(usize, FoldSnapshot)],
    prev_bsnaps: &[(usize, BracketSnapshot)],
    edit: Option<(usize, usize, i64)>,
    multi: bool,
) -> (
    Vec<Fold>,
    Vec<BracketPair>,
    Vec<BracketChar>,
    Vec<usize>,
    Vec<usize>,
    Vec<(usize, FoldSnapshot)>,
    Vec<(usize, BracketSnapshot)>,
)
where
    F: Fn(usize) -> SharedString + Copy,
{
    // 超大文档（行数 > full_limit）保持「按需折叠」语义：候选/配对/字符/边界/快照一并清空，
    // 由手动折叠/点击按需提供（避免病理文档的 O(全文) 后台扫描与增量兜底退化）。
    if row_count > full_limit {
        #[cfg(test)]
        LAST_INCREMENTAL_SPANS.with(|c| c.set([0, 0, 0, 0]));
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
    }
    // 增量前提不满足（多编辑段累积 / 无编辑描述）→ 整篇扫描兜底（输出=整篇口径，含快照）。
    let Some((line_a, min_stop, delta)) = edit.filter(|_| !multi) else {
        #[cfg(test)]
        LAST_INCREMENTAL_SPANS.with(|c| c.set([0, row_count, 0, row_count]));
        return full_fold_brackets(line, row_count);
    };

    // —— fold 候选 ——
    // 起点 = 最近「栈清空边界」与最近「入口栈快照」较靠后的一者（见 `pick_start`）；
    // 快照起点还原真态重扫、无需等栈空 → 根治「栈永不清空」文档的包络退化（⑦）。
    let (bf, bf_is_snapshot) = pick_start(prev_fb, prev_fsnaps, line_a);
    let fr_init = if bf_is_snapshot {
        prev_fsnaps
            .iter()
            .rev()
            .find(|(l, _)| *l == bf)
            .map(|(_, s)| s.clone())
    } else {
        None
    };
    // 汇聚检查点：受影响段（≥ min_stop 新坐标）之后的旧快照行映射到新坐标 → 重扫到
    // 该行若「新态 == 旧态」即停（⑦：栈影响已消解，尾部平移与整篇等价）。
    let merge_fs: Vec<(usize, &FoldSnapshot)> = prev_fsnaps
        .iter()
        .filter(|(l, _)| (*l as i64 + delta) >= min_stop as i64)
        .map(|(l, s)| ((*l as i64 + delta) as usize, s))
        .collect();
    let fr = scan_fold_segment(line, row_count, bf, Some(min_stop), fr_init, &merge_fs);
    let stop_old_f = (fr.stop as i64 - delta).max(0) as usize;
    let mut folds: Vec<Fold> = Vec::with_capacity(prev_folds.len());
    if bf_is_snapshot {
        // 快照起点（入口态非空）：存在「start 在 B 前/内、end 在 B 后」的**跨包络项**。
        // end 在尾段（> stop_old）→ 平移保留（start 未变：有效停点下块必在编辑前开启）；
        // end ≤ stop_old 的由重扫以受影响段口径重发（含 start < B 的旧块——重扫首态已还原）。
        for &f in prev_folds {
            if f.end > stop_old_f {
                let ns = if f.start >= stop_old_f {
                    (f.start as i64 + delta).max(0) as usize
                } else {
                    f.start
                };
                let ne = ((f.end as i64 + delta) as usize).min(row_count);
                if ns < row_count && ne > ns + 1 {
                    folds.push(Fold { start: ns, end: ne });
                }
            } else if f.end < bf {
                folds.push(f); // 完全在编辑前解析：原样保留
            }
            // 其余（end ∈ [bf, stop_old]）由重扫重发。
        }
    } else {
        // 空栈边界起点：无跨包络项（start < bf 的块必在 bf 前解析）→ 既有语义。
        for &f in prev_folds {
            if f.start < bf {
                folds.push(f); // 完全在编辑之前：原样保留
            } else if f.start >= stop_old_f {
                // 完全在受影响段之后：尾部行号平移
                let ns = (f.start as i64 + delta).max(0) as usize;
                let ne = ((f.end as i64 + delta) as usize).min(row_count);
                if ns < row_count && ne > ns + 1 {
                    folds.push(Fold { start: ns, end: ne });
                }
            }
            // [bf, stop_old_f) 内的旧项被重扫结果替换（下面并入）。
        }
    }
    folds.extend_from_slice(&fr.folds);
    let mut new_fb: Vec<usize> = Vec::with_capacity(prev_fb.len());
    new_fb.extend(prev_fb.iter().copied().filter(|&b| b < bf));
    new_fb.extend_from_slice(&fr.boundaries);
    // 边界与 fold 不同：段扫描在失效后又补记了 stop 行本身 → 旧边界 `== stop_old`
    // （映射到同一 stop 行）会被段扫描边界覆盖，只平移**严格大于** stop_old 的尾边界。
    new_fb.extend(
        prev_fb
            .iter()
            .copied()
            .filter(|&b| b > stop_old_f)
            .map(|b| (b as i64 + delta).max(0) as usize)
            .filter(|&b| b < row_count),
    );
    // fold 快照网格：起点前保留、受影响段由重扫补记、尾段平移（同边界合并口径）。
    let mut new_fsnaps: Vec<(usize, FoldSnapshot)> = Vec::with_capacity(prev_fsnaps.len());
    new_fsnaps.extend(prev_fsnaps.iter().filter(|(l, _)| *l < bf).cloned());
    new_fsnaps.extend(fr.snapshots);
    new_fsnaps.extend(
        prev_fsnaps
            .iter()
            .filter(|(l, _)| *l > stop_old_f)
            .map(|(l, s)| ((*l as i64 + delta).max(0) as usize, s.clone())),
    );

    // —— 括号配对 ——
    let (mut bb, bb_is_snapshot) = pick_start(prev_pb, prev_bsnaps, line_a);
    // 编辑恰落在**空栈边界**行（bb == line_a，K160 修复回归）：边界行自身承担「闭合
    // bb 之前残差开口」的闭号，从 bb 空栈重扫**造不出** → 回退前一边界（既有行为）。
    // 起点若已是**快照**（入口态含残差开口）→ 无需回退：直接从 bb 重扫被编辑行。
    if !bb_is_snapshot && bb == line_a && bb > 0 {
        let n = prev_pb.partition_point(|&b| b < line_a);
        bb = if n > 0 { prev_pb[n - 1] } else { 0 };
    }
    let br_init = if bb_is_snapshot {
        prev_bsnaps
            .iter()
            .rev()
            .find(|(l, _)| *l == bb)
            .map(|(_, s)| s.clone())
    } else {
        None
    };
    let merge_bs: Vec<(usize, &BracketSnapshot)> = prev_bsnaps
        .iter()
        .filter(|(l, _)| (*l as i64 + delta) >= min_stop as i64)
        .map(|(l, s)| ((*l as i64 + delta) as usize, s))
        .collect();
    let br = scan_bracket_segment(line, row_count, bb, Some(min_stop), br_init, &merge_bs);
    if br.truncated {
        // 括号扫描预算耗尽（安全护栏）→ 整体返回空括对/字符/边界（消费者 partition_point
        // 空表 → None → 无高亮，宁缺毋残）；折叠候选不受括对截断影响。
        #[cfg(test)]
        LAST_INCREMENTAL_SPANS.with(|c| c.set([bf, fr.stop, bb, br.stop]));
        return (
            folds,
            Vec::new(),
            Vec::new(),
            new_fb,
            Vec::new(),
            new_fsnaps,
            Vec::new(),
        );
    }
    let stop_old_b = (br.stop as i64 - delta).max(0) as usize;
    let mut pairs: Vec<BracketPair> = Vec::with_capacity(prev_pairs.len());
    if bb_is_snapshot {
        // 快照起点（入口态非空）：同 folds——按闭号落点精确合并（K160）。
        for &p in prev_pairs {
            if p.close_line > stop_old_b {
                // 闭号在尾段：整对在尾（open ≥ stop_old → 开/闭都平移）或自包络跨入尾段
                // （open < stop_old → 开行不变：有效停点下该 open 必在编辑前开启）。
                let no = if p.open_line >= stop_old_b {
                    (p.open_line as i64 + delta).max(0) as usize
                } else {
                    p.open_line
                };
                let nc = ((p.close_line as i64 + delta) as usize).min(row_count);
                if no < row_count && nc > no {
                    let mut np = BracketPair {
                        open_line: no,
                        close_line: nc,
                        ..p
                    };
                    // 跨包络平移（open < stop_old、开行未变、闭行平移）：体内行含受影响段、
                    // 内容已受编辑影响 → `guide_body_min_units` 旧值可能陈旧，须用**新文档**
                    // 行内容重算（与整篇扫描对新增字段逐位一致，K160；其余字段只依赖开/闭
                    // 行内容与列、平移下不变，无需重算）。空栈起点无跨包络项（§上文），不涉及。
                    if p.open_line < stop_old_b {
                        np.guide_body_min_units = compute_guide_body_min_units(&line, no, nc);
                    }
                    pairs.push(np);
                }
            } else if p.close_line < bb {
                pairs.push(p); // 完全在编辑前解析（重扫首态不含其闭号）→ 原样保留
            }
            // 其余（闭 ∈ [bb, stop_old]）由重扫重发。
        }
    } else {
        // 空栈边界起点：无跨包络括对 → 既有语义。
        for &p in prev_pairs {
            if p.open_line < bb {
                pairs.push(p); // 完全在编辑之前：原样保留
            } else if p.open_line >= stop_old_b {
                // 完全在受影响段之后：尾部行号平移（开/闭行一起移）
                let no = (p.open_line as i64 + delta).max(0) as usize;
                let nc = ((p.close_line as i64 + delta) as usize).min(row_count);
                if no < row_count && nc > no {
                    pairs.push(BracketPair {
                        open_line: no,
                        close_line: nc,
                        ..p
                    });
                }
            }
            // [bb, stop_old_b) 内的旧项被重扫结果替换（下面并入）。
        }
    }
    pairs.extend_from_slice(&br.pairs);
    let mut new_pb: Vec<usize> = Vec::with_capacity(prev_pb.len());
    new_pb.extend(prev_pb.iter().copied().filter(|&b| b < bb));
    new_pb.extend_from_slice(&br.boundaries);
    // 同 new_fb：段扫描已在受影响包络内重记了边界（含 stop 行），只平移严格大于
    // stop_old 的尾边界，避免与段扫描边界重复。
    new_pb.extend(
        prev_pb
            .iter()
            .copied()
            .filter(|&b| b > stop_old_b)
            .map(|b| (b as i64 + delta).max(0) as usize)
            .filter(|&b| b < row_count),
    );
    // 括号快照网格：同 new_fsnaps 口径。
    let mut new_bsnaps: Vec<(usize, BracketSnapshot)> = Vec::with_capacity(prev_bsnaps.len());
    new_bsnaps.extend(prev_bsnaps.iter().filter(|(l, _)| *l < bb).cloned());
    new_bsnaps.extend(br.snapshots);
    new_bsnaps.extend(
        prev_bsnaps
            .iter()
            .filter(|(l, _)| *l > stop_old_b)
            .map(|(l, s)| ((*l as i64 + delta).max(0) as usize, s.clone())),
    );

    // —— 括号字符（按 line 仿 pairs 三段连续有序合并）——
    // 起点行的特殊性：`bb` = 首个「栈空」边界行，其**前**可能仍有未闭开口跨入 bb 行
    // （如 `fn x() {` + 下一行 `}`：`}` 在 bb 行闭合了 bb 之前的 `{`）。从 bb 以全新
    // 状态重扫**造不出**这些闭合字符（栈空 ⇒ 视为不匹配跳过），且 bb 行自身的开口深度
    // 也依赖既有残差栈 → bb 行字符不可由重扫复现。对策：编辑行**严格在 bb 之后**
    // （`bb < line_a`）时，bb 行内容取自上次落地（prev，即整篇口径），重扫的 bb 行
    // 字符丢弃；编辑恰好在 bb 行（`bb == line_a`）时 bb 行已被编辑 → 用重扫结果。
    // 字符是**单行物理标点**（深浅只由行前状态决定、不构成跨包络项）→ 起点为快照时
    // 三条分支同边界口径（快照入口态已还原、重扫能复现起点行，但起点行未受编辑时
    // prev 即真值 → 保留 prev、滤重扫消重复；起点行受编辑 [bb==line_a] 时用重扫）。
    let mut chars: Vec<BracketChar> = Vec::with_capacity(prev_chars.len() + br.chars.len());
    let keep_bb_line = bb < line_a;
    for &c in prev_chars {
        if c.line < bb || (keep_bb_line && c.line == bb) {
            chars.push(c); // 完全在编辑之前 / 未受编辑的起点行：原样保留
        } else if c.line > stop_old_b {
            // 完全在受影响段之后：尾部行号平移（与 pairs 同口径），越界剔除。
            // 注意**严格 > stop_old_b**：`== stop_old_b` 的旧行内容平移后落在
            // `stop_old_b + delta = stop`，恰被重扫的 stop 行覆盖（重扫该行 =
            // 权威，且深度的旧值可能因编辑陈旧）——丢弃交由重扫补齐，否则与新行
            // stop 重复。
            let nl = (c.line as i64 + delta).max(0) as usize;
            if nl < row_count {
                chars.push(BracketChar { line: nl, ..c });
            }
        }
        // [bb, stop_old_b) 内的旧项被重扫结果替换（下面并入；bb 行在 keep_bb_line 时
        // 上面已保留、重扫 bb 行字符在下面过滤掉；== stop_old_b 的旧行同样由重扫
        // 的 stop 行补齐）。
    }
    chars.extend(
        br.chars
            .iter()
            .copied()
            .filter(|c| !keep_bb_line || c.line > bb),
    );

    #[cfg(test)]
    LAST_INCREMENTAL_SPANS.with(|c| c.set([bf, fr.stop, bb, br.stop]));
    (folds, pairs, chars, new_fb, new_pb, new_fsnaps, new_bsnaps)
}

/// 单轮「折叠候选 + 括号配对 + 括号字符」重算的**统一计算入口**（供后台刷新与测试同步
/// 落地共用，避免两处口径漂移）：返回 `(folds, pairs, chars, fold_boundaries,
/// bracket_boundaries, fold_snapshots, bracket_snapshots, was_incremental)`。
/// `was_incremental` = 本次是否走了增量路径（受影响段局部重扫 + 尾部平移），`false`
/// 表示整篇扫描兜底（多编辑段累积 / 无编辑描述 / 超行数清空）。
/// 可见性：`editor`（视图层）的 `document.rs`/`editing.rs` 跨 crate 消费 → 转 pub。
pub fn compute_fold_brackets<F>(
    line: F,
    row_count: usize,
    full_limit: usize,
    prev_folds: &[Fold],
    prev_pairs: &[BracketPair],
    prev_chars: &[BracketChar],
    prev_fb: &[usize],
    prev_pb: &[usize],
    prev_fsnaps: &[(usize, FoldSnapshot)],
    prev_bsnaps: &[(usize, BracketSnapshot)],
    pending: Option<(usize, usize, i64)>,
    multi: bool,
) -> (
    Vec<Fold>,
    Vec<BracketPair>,
    Vec<BracketChar>,
    Vec<usize>,
    Vec<usize>,
    Vec<(usize, FoldSnapshot)>,
    Vec<(usize, BracketSnapshot)>,
    bool,
)
where
    F: Fn(usize) -> SharedString + Copy,
{
    let was_incremental = pending.is_some() && !multi && row_count <= full_limit;
    let (folds, pairs, chars, fb, pb, fsnaps, bsnaps) = incremental_fold_brackets(
        line,
        row_count,
        full_limit,
        prev_folds,
        prev_pairs,
        prev_chars,
        prev_fb,
        prev_pb,
        prev_fsnaps,
        prev_bsnaps,
        pending,
        multi,
    );
    (folds, pairs, chars, fb, pb, fsnaps, bsnaps, was_incremental)
}

/// 平坦（互不相交、按 `start` 升序）折叠集合 + 视觉↔逻辑映射。
#[derive(Clone, Debug, Default)]
pub struct FoldMap {
    ranges: Vec<Fold>,
    /// `hidden_prefix[i]` = 前 `i+1` 个折叠累计隐藏行数。
    hidden_prefix: Vec<usize>,
    /// 逻辑总行数（编辑器行数，`set_total`/`sync_edit` 维护）。
    total: usize,
}

impl FoldMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清空全部折叠（文档整篇替换/打开前调用）。
    pub fn clear(&mut self) {
        self.ranges.clear();
        self.hidden_prefix.clear();
        self.total = 0;
    }

    /// 同步逻辑总行数（编辑器行数变化后），同时把越界的折叠裁掉。
    pub fn set_total(&mut self, total: usize) {
        self.total = total.max(1);
        self.rebuild();
    }

    /// 当前逻辑总行数。
    pub fn total(&self) -> usize {
        self.total
    }

    /// 视觉总行数（O(1)）。
    pub fn visual_line_count(&self) -> usize {
        self.total
            .saturating_sub(self.hidden_prefix.last().copied().unwrap_or(0))
    }

    /// 逻辑行 ∈ [start, end) 的折叠（含首行自身）；无则 `None`。
    pub fn fold_at(&self, line: usize) -> Option<&Fold> {
        let i = self.ranges.partition_point(|f| f.start <= line);
        let i = i.checked_sub(1)?;
        let f = &self.ranges[i];
        (line < f.end).then_some(f)
    }

    /// 以 `line` 为折叠首行的折叠（= 该行渲染「已折叠」指示器）。
    pub fn fold_starting_at(&self, line: usize) -> Option<&Fold> {
        match self.fold_at(line) {
            Some(f) if f.start == line => Some(f),
            _ => None,
        }
    }

    /// `line` 是否处于某折叠的**隐藏体内部**（严格 `start < line < end`）。
    pub fn hidden_containing(&self, line: usize) -> Option<&Fold> {
        match self.fold_at(line) {
            Some(f) if line > f.start => Some(f),
            _ => None,
        }
    }

    /// 逻辑行 → 视觉行（隐藏体内部塌缩到折叠首行的视觉行）。
    pub fn visual_row(&self, logical: usize) -> usize {
        // 最后一个 start <= logical 的折叠。
        if let Some(f) = self.fold_at(logical) {
            // logical ∈ [f.start, f.end)：首行直接等于自身，隐藏体塌缩到首行。
            let i = self.ranges.partition_point(|x| x.start <= logical) - 1;
            let before = self.hidden_prefix.get(i.wrapping_sub(1)).copied().unwrap_or(0);
            return f.start - before;
        }
        // 折叠体之前/之后：会计入所有「已完整结束」的折叠（end <= logical）。
        let j = self.ranges.partition_point(|f| f.end <= logical);
        let hidden = self.hidden_prefix.get(j.wrapping_sub(1)).copied().unwrap_or(0);
        logical.saturating_sub(hidden)
    }

    /// 视觉行 → 逻辑行（单调递增；映射到可见行）。
    pub fn logical_row(&self, visual: usize) -> usize {
        let vcount = self.visual_line_count();
        if vcount == 0 {
            return 0;
        }
        let visual = visual.min(vcount.saturating_sub(1));
        // 最后一个「折叠首行视觉位置 <= visual」的折叠。
        let n = self.ranges.len();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.visual_start(mid) <= visual {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return visual;
        }
        let idx = lo - 1;
        if visual == self.visual_start(idx) {
            // 视觉恰落在折叠首行 → 该首行的逻辑行。
            self.ranges[idx].start
        } else {
            // 折叠体在视觉上是「塌缩的一格」：其后的视觉行跳过整个隐藏体。
            visual + self.hidden_prefix[idx]
        }
    }

    /// 折叠 `i` 的首行在视觉坐标系上的位置。
    fn visual_start(&self, i: usize) -> usize {
        let before = if i == 0 { 0 } else { self.hidden_prefix[i - 1] };
        self.ranges[i].start.saturating_sub(before)
    }

    /// 切换 `[start, end)`（半开逻辑行）的折叠：
    /// - 已有以 `start` 为折叠首行的折叠 → 展开（删除）；
    /// - 否则删除所有与之相交的折叠后新建（保持平坦）。
    pub fn toggle(&mut self, start: usize, end: usize) {
        if self.total == 0 {
            self.total = 1;
        }
        // 展开：start 行已是折叠首行。
        if let Some(pos) = self.ranges.iter().position(|f| f.start == start) {
            self.ranges.remove(pos);
            self.rebuild();
            return;
        }
        // 无可隐藏行（end <= start+1）→ 无事发生（也不能创建空折叠）。
        if end <= start + 1 {
            return;
        }
        let start = start.min(self.total.saturating_sub(1));
        let end = end.min(self.total);
        if end <= start + 1 {
            return;
        }
        // 平坦化：去掉与 [start, end) 相交的既有折叠。
        self.ranges.retain(|f| !(f.end > start && f.start < end));
        self.ranges.push(Fold { start, end });
        self.rebuild();
    }

    /// 展开包含 `line` 的折叠（含折叠首行自身）。
    pub fn expand_at(&mut self, line: usize) {
        if let Some(f) = self.fold_at(line).copied() {
            self.ranges.retain(|x| x.start != f.start);
            self.rebuild();
        }
    }

    /// 文本编辑后同步折叠区间（**必须在 Buffer 已应用编辑后、且给出编辑前受影响
    /// 的行跨度时调用**）：
    /// - 被替换的行跨度 `[line_a, line_b]`（含两端，编辑前坐标）；
    /// - `new_text`（替换后的新文本，用于算净行增量）；
    /// - `new_total`（编辑后的逻辑总行数）。
    ///
    /// 规则：
    /// 1. 折叠**隐藏体**与编辑跨度相交 → 自动展开（丢弃该折叠）；
    /// 2. 折叠**首行**落在编辑跨度内 → 锚消失，丢弃；
    /// 3. 折叠整体在编辑之前 → 原样保留；
    /// 4. 折叠整体在编辑之后 → 按净行增量平移（越界截断/丢弃）。
    pub fn sync_edit(&mut self, line_a: usize, line_b: usize, new_text: &str, new_total: usize) {
        self.total = new_total.max(1);
        let insert_lines = new_text.matches('\n').count();
        let remove_lines = line_b.saturating_sub(line_a);
        let delta = insert_lines as isize - remove_lines as isize;
        let mut keep: Vec<Fold> = Vec::with_capacity(self.ranges.len());
        for f in self.ranges.drain(..) {
            // 隐藏体 (f.start, f.end) 与 [line_a, line_b] 相交？
            // 相交 ⟺ 存在 L：f.start < L < f.end 且 line_a <= L <= line_b。
            if f.start < line_b && f.end > line_a {
                continue; // 编辑进了折叠体 → 展开
            }
            // 折叠首行被替换。
            if f.start >= line_a && f.start <= line_b {
                continue;
            }
            // 折叠整体在编辑之前。
            if f.end <= line_a {
                keep.push(f);
                continue;
            }
            // 折叠整体在编辑之后（f.start > line_b 由上面排除）→ 平移。
            let ns = (f.start as isize + delta).max(0) as usize;
            let ne = (f.end as isize + delta).max(0) as usize;
            let ne = ne.min(self.total);
            if ne > ns + 1 && ns < self.total {
                keep.push(Fold { start: ns, end: ne });
            }
        }
        self.ranges = keep;
        self.rebuild();
    }

    /// 重建前缀并裁剪越界/退化区间（保持平坦有序）。文档收缩（`set_total`）时把
    /// 越界折叠的 `end` **截断**到新总行数（而非整条丢弃），保留仍有效的折叠。
    fn rebuild(&mut self) {
        self.ranges.retain(|f| f.start < self.total);
        for f in &mut self.ranges {
            if f.end > self.total {
                f.end = self.total;
            }
        }
        self.ranges.retain(|f| f.end > f.start + 1);
        self.ranges.sort_by_key(|f| f.start);
        let mut prefix = Vec::with_capacity(self.ranges.len());
        let mut acc = 0usize;
        for f in &self.ranges {
            acc += f.body_len();
            prefix.push(acc);
        }
        self.hidden_prefix = prefix;
    }

    /// 全部折叠的只读视图（测试/诊断）。
    pub fn folds(&self) -> &[Fold] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造：total 行 + 若干折叠 `(start, end)`。
    fn fm(total: usize, folds: &[(usize, usize)]) -> FoldMap {
        let mut m = FoldMap::new();
        m.set_total(total);
        for &(s, e) in folds {
            m.toggle(s, e);
        }
        m
    }

    #[test]
    fn toggle_and_visual_count() {
        let mut m = FoldMap::new();
        m.set_total(12);
        // 折叠 [5,10)：隐藏 6..9 共 4 行。
        m.toggle(5, 10);
        assert_eq!(m.visual_line_count(), 12 - 4);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
        // 再 toggle 同 start → 展开。
        m.toggle(5, 10);
        assert_eq!(m.visual_line_count(), 12);
        assert!(m.folds().is_empty());
        // 空折叠不可创建。
        m.toggle(6, 7);
        assert!(m.folds().is_empty());
    }

    /// 落地期预存字段 `guide_body_min_units: Option<u32>` 的内存量级硬护栏（K82 姿势）：
    /// `BracketPair` 48B→56B（+8B/对）、`BracketPairLite` 40B→48B。上界估算：预算
    /// `BRACKET_CHAR_MAX`＝2^20 对全程驻留增量 = 2^20 × 8B = 8 MiB（相对既有 ~48 MiB
    /// → +16.7%，同数量级内、不膨胀还款）。改字段类型先改此护栏。
    #[test]
    fn bracket_pair_size_guarded() {
        assert_eq!(
            std::mem::size_of::<BracketPair>(),
            56,
            "BracketPair 应 48B→56B（+8B/对 = Option<u32>）；实测 {}",
            std::mem::size_of::<BracketPair>()
        );
        assert_eq!(
            std::mem::size_of::<BracketPairLite>(),
            48,
            "BracketPairLite 应 40B→48B（+8B = Option<u32>）；实测 {}",
            std::mem::size_of::<BracketPairLite>()
        );
    }

    #[test]
    fn toggle_removes_overlaps() {
        let mut m = fm(20, &[(2, 8), (10, 15)]);
        // 新建 [3,12) 与两者相交 → 全删后只留新建。
        m.toggle(3, 12);
        assert_eq!(m.folds(), &[Fold { start: 3, end: 12 }]);
    }

    #[test]
    fn visual_logical_roundtrip_flat() {
        let m = fm(12, &[(5, 10)]);
        // 逻辑 → 视觉
        for l in 0..=4 {
            assert_eq!(m.visual_row(l), l);
        }
        assert_eq!(m.visual_row(5), 5); // 首行可见
        for l in 6..=9 {
            assert_eq!(m.visual_row(l), 5, "隐藏体塌缩到首行 l={l}");
        }
        assert_eq!(m.visual_row(10), 6, "end 行可见、跳过隐藏体");
        assert_eq!(m.visual_row(11), 7);
        // 视觉 → 逻辑
        for v in 0..=5 {
            assert_eq!(m.logical_row(v), v);
        }
        assert_eq!(m.logical_row(6), 10);
        assert_eq!(m.logical_row(7), 11);
    }

    #[test]
    fn multiple_folds_mapping() {
        let m = fm(30, &[(3, 6), (10, 20)]);
        // 折叠1 隐藏 4,5（2 行）；折叠2 隐藏 11..19（9 行）。
        assert_eq!(m.visual_line_count(), 30 - 11);
        assert_eq!(m.visual_row(3), 3);
        assert_eq!(m.visual_row(5), 3);
        assert_eq!(m.visual_row(6), 4);
        assert_eq!(m.visual_row(10), 8); // 10 - hidden(折叠1=2)
        assert_eq!(m.visual_row(30 - 1), 30 - 11 - 1);
        assert_eq!(m.logical_row(0), 0);
        assert_eq!(m.logical_row(3), 3);
        assert_eq!(m.logical_row(4), 6);
        assert_eq!(m.logical_row(8), 10);
        // 视觉 9 = 折叠2 首行（10）之后的首个可见行 = 跳过折叠体 11..19 → 逻辑 20。
        assert_eq!(m.logical_row(9), 20);
        // 单调映射往返：对每个视觉行，visual_row(logical_row(v)) == v。
        for v in 0..m.visual_line_count() {
            let l = m.logical_row(v);
            assert_eq!(m.visual_row(l), v, "roundtrip v={v} l={l}");
        }
    }

    #[test]
    fn sync_edit_outside_keeps_and_shifts() {
        let mut m = fm(12, &[(5, 10)]);
        // 在第 2 行插入一行（line_a=line_b=2，new_text 含换行，delta=+1）。
        m.sync_edit(2, 2, "\nX", 13);
        assert_eq!(m.folds(), &[Fold { start: 6, end: 11 }]);
        // 撤销回插：删除第 2 起的整行文本（编辑前行跨度 [2, 3)，delta=-1）。
        m.sync_edit(2, 3, "", 12);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
        // 在折叠之后删除一行（delta=-1，折叠整体在其前 → 原样保留）。
        m.sync_edit(11, 12, "", 11);
        assert_eq!(m.folds(), &[Fold { start: 5, end: 10 }]);
    }

    #[test]
    fn sync_edit_inside_body_expands() {
        let mut m = fm(12, &[(5, 10)]);
        // 编辑隐藏在 7 行的内容 → 折叠展开。
        m.sync_edit(7, 7, "a", 12);
        assert!(m.folds().is_empty());
    }

    #[test]
    fn sync_edit_on_start_line_drops_fold() {
        let mut m = fm(12, &[(5, 10)]);
        // 编辑折叠首行（整行被替换）→ 锚消失，丢弃。
        m.sync_edit(5, 5, "zzz", 12);
        assert!(m.folds().is_empty());
    }

    #[test]
    fn sync_edit_deleting_body_removes_and_shifts_after() {
        let mut m = fm(20, &[(5, 10), (15, 18)]);
        // 删除整行跨度 [4, 10)（覆盖折叠1 的 5..9）：折叠1 消失；
        // 净 delta = 0 − (10−4) = −6 → 折叠2（start15>10）平移为 [9,12]。
        m.sync_edit(4, 10, "", 14);
        assert_eq!(m.folds(), &[Fold { start: 9, end: 12 }]);
    }

    #[test]
    fn set_total_clamps_ranges() {
        let mut m = fm(20, &[(5, 10), (15, 18)]);
        m.set_total(9); // 文档变短：折叠2 越界被裁，折叠1 end 截到 9。
        assert_eq!(m.folds(), &[Fold { start: 5, end: 9 }]);
    }

    #[test]
    fn hidden_containing_queries() {
        let m = fm(12, &[(5, 10)]);
        assert!(m.fold_starting_at(5).is_some());
        assert!(m.fold_starting_at(6).is_none());
        assert!(m.hidden_containing(5).is_none()); // 首行不是隐藏体
        assert!(m.hidden_containing(6).is_some());
        assert!(m.hidden_containing(9).is_some());
        assert!(m.hidden_containing(10).is_none()); // end 行可见
        assert_eq!(m.fold_at(7).map(|f| f.end), Some(10));
        assert_eq!(m.fold_at(11), None);
    }

    // ------------------------------------------------------------------
    // 括号配对扫描（括号标尺的数据源）
    // ------------------------------------------------------------------

    /// 按行号返回行文本的闭包（越界给空行）。
    fn line_fn<'a>(lines: &'a [&'a str]) -> impl Fn(usize) -> SharedString + Copy + 'a {
        move |i: usize| {
            lines
                .get(i)
                .map(|s| SharedString::from(*s))
                .unwrap_or_else(|| SharedString::new_static(""))
        }
    }

    #[test]
    fn bracket_scan_nested_and_multi_line_only() {
        let lines = [
            "impl RowData {",
            "    fn empty() -> Self {",
            "        Self {",
            "            a,",
            "        }",
            "    }",
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 只记跨行：内层 Self{, 中 fn{, 外层 impl{ 各一条；
        // 竖线列 = 块首行缩进宽（0/4/8 单位）；depth = 嵌套 DFS 深度（0 起）。
        assert_eq!(
            pairs,
            vec![
                BracketPair { open_line: 2, open_col: 13, close_line: 4, close_col: 8, guide_units: 8, guide_body_min_units: Some(12), depth: 2 },
                BracketPair { open_line: 1, open_col: 23, close_line: 5, close_col: 4, guide_units: 4, guide_body_min_units: Some(8), depth: 1 },
                BracketPair { open_line: 0, open_col: 13, close_line: 6, close_col: 0, guide_units: 0, guide_body_min_units: Some(4), depth: 0 },
            ]
        );
        // 单行配对不产竖线。
        let one = ["fn a() {}"];
        assert!(scan_bracket_pairs(line_fn(&one), 1).is_empty());
    }

    #[test]
    fn bracket_scan_skips_strings_and_comments() {
        let lines = [
            "let s = \"{ not(open }\";",       // 字符串里的括号忽略
            "// { comment bracket }",          // 行注释忽略
            "/* block { ( */",                 // 块注释开始
            "fn ok() {",                       // 真正的开括号
            "/* } still comment */",           // 闭括号在注释里 → 忽略
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 开行 `fn ok() {` 缩进 0 → 竖线列 0（块注释内的 `}` 已被跳过）；depth = 0（首个开括号）。
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 3, open_col: 8, close_line: 5, close_col: 0, guide_units: 0, guide_body_min_units: Some(0), depth: 0 }]
        );
        // 转义字符串 `"a\"{b"` 内部花括号忽略；块体 `    x();` → 竖线列 4。
        let esc = ["let s = \"a\\\"{b\";\nfn f() {\n    x();\n}"];
        let el: Vec<&str> = esc[0].split('\n').collect();
        let pairs = scan_bracket_pairs(line_fn(&el), el.len());
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 1, open_col: 7, close_line: 3, close_col: 0, guide_units: 0, guide_body_min_units: Some(4), depth: 0 }],
            "字符串内转义花括号应被跳过"
        );
    }

    #[test]
    fn bracket_scan_lifetime_does_not_break() {
        let lines = ["fn max<'a>(x: &'a str, y: &'a str) &'a str {", "    x", "}"];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        // 生命周期撇号不当作字符字面量：`{` 在 ( ) 之后仍配对；开行缩进 0；depth = 0。
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 0, open_col: 43, close_line: 2, close_col: 0, guide_units: 0, guide_body_min_units: Some(4), depth: 0 }]
        );
    }

    // ------------------------------------------------------------------
    // 括号字符层级表（括号文本着色，对齐 VS Code bracketPairColorization）
    // ------------------------------------------------------------------

    /// 逐字符：单行配对（`{}`/`()`）与跨行配对的开/闭括号**都**记入 `chars`，按
    /// `(line,col)` 升序；depth = 开括号压栈前 nest_depth、闭括号 = 对应开口的 depth。
    ///
    /// 字节列演算（一行一算，col 皆字节下标）：
    /// - 行 0 `fn f() {`：`(`4、`)`5、`{`7；
    /// - 行 1 `    let x = { a: {b}, };`：`{`12、`{`17、`}`19、`}`22（`"    let x = { a: {b}, };"`：4 空格后 let=4..7, x=8, =10, {12, {17, }19, }22）；
    /// - 行 2 `    g();`：`(`5、`)`6；行 3 `}`0。
    #[test]
    fn scan_bracket_chars_single_and_cross_line_ordered() {
        let lines = [
            "fn f() {",                   // 行 0：`(` `)` 单行配对 d0；`{`(7) d0 开块
            "    let x = { a: {b}, };",   // 行 1：`{`(12) d1、`{`(17) d2、`}`(19) d2、`}`(22) d1
            "    g();",                   // 行 2：`(`(5) d1、`)`(6) d1
            "}",                          // 行 3：`}`(0) d0 闭块
        ];
        let report = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        assert_eq!(
            report.chars,
            vec![
                BracketChar { line: 0, col: 4, depth: 0 },
                BracketChar { line: 0, col: 5, depth: 0 },
                BracketChar { line: 0, col: 7, depth: 0 },
                BracketChar { line: 1, col: 12, depth: 1 },
                BracketChar { line: 1, col: 17, depth: 2 },
                BracketChar { line: 1, col: 19, depth: 2 },
                BracketChar { line: 1, col: 22, depth: 1 },
                BracketChar { line: 2, col: 5, depth: 1 },
                BracketChar { line: 2, col: 6, depth: 1 },
                BracketChar { line: 3, col: 0, depth: 0 },
            ],
            "单行+跨行开闭均逐字符记录且按 (line,col) 升序: {:?}",
            report.chars
        );
        // 跨行配对（竖线）与字符表互补：字符数 > 配对数的 2×（单行配对两端只进字符表）。
        assert!(report.chars.len() > report.pairs.len() * 2, "字符表应含单行配对项");
    }

    /// 字符串/注释内的括号字符**不**进入字符表（与配对扫描同一词法兜底）。
    /// 字节列演算：`fn ok() {` → `(`5、`)`6、`{`8。
    #[test]
    fn scan_bracket_chars_skips_strings_and_comments() {
        let lines = [
            "let s = \"{ not(open }\";", // 字符串里的括号忽略
            "// { comment bracket }",    // 行注释忽略
            "/* block { ( */",           // 块注释开始
            "fn ok() {",                 // 真正的开括号
            "/* } still comment */",     // 注释内闭括号忽略
            "}",
        ];
        let report = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        // 只应有 `fn ok() {` 的 `(` `)` `{`（行 3）与行 5 的 `}`：字符串/注释内一律无。
        assert_eq!(
            report.chars,
            vec![
                BracketChar { line: 3, col: 5, depth: 0 }, // (
                BracketChar { line: 3, col: 6, depth: 0 }, // )
                BracketChar { line: 3, col: 8, depth: 0 }, // {
                BracketChar { line: 5, col: 0, depth: 0 }, // } 配 fn ok 的 {
            ]
        );
        // 转义字符串 `"a\"{b"` 内部花括号同样忽略。
        let esc = "let s = \"a\\\"{b\";\nfn f() {\n    x();\n}";
        let el: Vec<&str> = esc.split('\n').collect();
        let er = scan_bracket_segment(line_fn(&el), el.len(), 0, None, None, &[]);
        assert!(
            er.chars.iter().all(|c| c.line != 0),
            "字符串内转义花括号不应进字符表: {:?}",
            er.chars
        );
    }

    /// 不匹配的闭括号（栈空/层空）**跳过**——只记成功配对的闭括号。
    /// 深度演算：行 2 `{`(d0) 开后被行 4 `}`(d0) 闭合 → nest 归 0 → 行 5 `fn g() {` 的
    /// `(`4、`)`5、`{`7 全为 d0（"fn g() {"：f0 n1 ' '2 g3 '('4 ')'5 ' '6 '{'7），
    /// 行 6 `}`(0) 配它。
    #[test]
    fn scan_bracket_chars_unmatched_close_skipped() {
        let lines = [
            "}",          // 行 0：孤立 `}`（各栈空）→ 不记
            "x = ]",      // 行 1：孤立 `]`（括号栈空）→ 不记
            "{",          // 行 2：永不匹配的开口 `{` → 记（depth0）
            "",           // 行 3
            "}",          // 行 4：`}` 配行 2 的 `{` → 记（depth0）
            "fn g() {",   // 行 5
            "}",          // 行 6：`}` 配 fn g 的 `{` → 记
        ];
        let report = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        assert_eq!(
            report.chars,
            vec![
                BracketChar { line: 2, col: 0, depth: 0 },
                BracketChar { line: 4, col: 0, depth: 0 },
                BracketChar { line: 5, col: 4, depth: 0 }, // fn g 内 `(`
                BracketChar { line: 5, col: 5, depth: 0 }, // `)`
                BracketChar { line: 5, col: 7, depth: 0 }, // `{`
                BracketChar { line: 6, col: 0, depth: 0 }, // `}` 配 fn g 的 `{`
            ],
            "孤立闭括号跳过、永不匹配开口保留: {:?}",
            report.chars
        );
    }

    // ------------------------------------------------------------------
    // C4 缩进感知配对（`}` 只匹配本缩进层最近未配 `{`；每层独立 LIFO；层空丢弃）
    // ------------------------------------------------------------------

    /// C4 配对：fixture（`target/guidefix.rs` 前 16 行）恰好 3 对——fn a、if x==1、if x1==1；
    /// mega 行 8 个 `{`（缩进 8 层）永不配对。预期为 LIFO 收栈序。
    #[test]
    fn scan_indent_aware_pairing_three_pairs() {
        let lines = [
            "fn a() {",
            "    let x = 1;",
            "    if x1 == 1 {",
            "        t1();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {",
            "    let x2 = 1;",
            "    if x == 1 {",
            "        y();",
            "        z();",
            "    }",
            "    w();",
            "}",
            "",
            "        z1();",
            "    }",
            "    t2();",
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        assert_eq!(pairs.len(), 3, "mega 8 开永不配对，应恰好 3 对: {pairs:?}");
        // LIFO 收栈序：行 8 先收 if x==1，行 10 收 fn a，行 13 收 if x1==1。
        // depth：mega 8 开（永不闭）把纯 DFS 计数器顶到 nest=10 → if x==1 的 { 记 depth 10；
        // fn a / if x1 分别在 nest=0 / 1 时压入 → 0 / 1（坏代码下计数器偏差，竖线纯视觉）。
        assert_eq!(
            pairs,
            vec![
                BracketPair { open_line: 5, open_col: 14, close_line: 8, close_col: 4, guide_units: 4, guide_body_min_units: Some(8), depth: 10 },
                BracketPair { open_line: 0, open_col: 7, close_line: 10, close_col: 0, guide_units: 0, guide_body_min_units: Some(4), depth: 0 },
                BracketPair { open_line: 2, open_col: 15, close_line: 13, close_col: 4, guide_units: 4, guide_body_min_units: Some(4), depth: 1 },
            ],
            "期待 ifx==1 / fn a / ifx1 三对（顺序为 LIFO 收栈序）"
        );
        // 未配对/孤立：mega 8 开、行 15 的 `}`（缩进 0 层空）均不落配对。
        assert!(!pairs.iter().any(|p| p.open_line == 3), "mega 行不得被配对: {pairs:?}");
        assert!(!pairs.iter().any(|p| p.close_line == 15), "行 15 的右花括号层空应丢弃: {pairs:?}");
    }

    /// C4 鲁棒：mega 单行 8 个 `{` 各成栈项（同层 LIFO），但只可能被**同缩进层**的 `}`
    /// 关闭；indent-4 的 `}` 层空即丢弃——绝不跨层偷配。另附同层 LIFO 对照。
    #[test]
    fn scan_mega_single_line_braces_no_steal() {
        // mega（缩进 8）8 开 + indent-4 闭：层空丢弃，只有 fn a 配对。
        let lines = [
            "fn a() {",
            "        t1();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {y();fn a() {",
            "    }",
            "}",
        ];
        let pairs = scan_bracket_pairs(line_fn(&lines), lines.len());
        assert_eq!(
            pairs,
            vec![BracketPair { open_line: 0, open_col: 7, close_line: 3, close_col: 0, guide_units: 0, guide_body_min_units: Some(8), depth: 0 }],
            "缩进 8 的 mega 8 开不得被缩进 4 的右花括号偷配: {pairs:?}"
        );
        // 同层 LIFO 对照：mega 与 `}` 同在缩进 4 层 → 最近一个 `{` 被关闭（同层正常配对一次）。
        let same = [
            "fn a() {",
            "    t1();fn a() {y();fn a() {y();fn a() {",
            "    }",
            "}",
        ];
        let sp = scan_bracket_pairs(line_fn(&same), same.len());
        assert_eq!(sp.len(), 2, "同层 3 开 + 1 闭 = 1 对 + fn a 对: {sp:?}");
        assert!(
            sp.iter().any(|p| p.open_line == 1 && p.close_line == 2),
            "同层最近一个左花括号被右花括号关闭: {sp:?}"
        );
    }

    /// C4 边界：断点 = 各缩进层花括号栈全空 && 括号栈空 && 不在块注释。三层嵌套全部配平后
    /// 抛边界；从该边界以全新状态续扫 ≡ 整篇扫描（增量断点契约）。
    #[test]
    fn boundary_check_all_indent_levels_empty() {
        let lines = [
            "fn a() {", // stack[0]
            "    {",    // stack[4]
            "        {", // stack[8]
            "        }", // 缩进 8 → pop
            "    }",    // 缩进 4 → pop
            "}",        // 缩进 0 → pop → 各层全空 → 边界
            "fn b() {",
            "    x();",
            "}",
        ];
        let full = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        assert!(
            full.boundaries.contains(&5),
            "三层嵌套配平后应抛边界: {:?}",
            full.boundaries
        );
        assert!(full.boundaries.contains(&8), "fn b 关闭后再抛边界: {:?}", full.boundaries);
        assert!(!full.boundaries.contains(&2), "stack[4] 未空 → 行 2 不是边界");
        // 从每个边界续扫 ≡ 整篇扫描（增量重算的断点前提）。
        for &b in &full.boundaries {
            let seg = scan_bracket_segment(line_fn(&lines), lines.len(), b, None, None, &[]);
            let expect_pairs: Vec<BracketPair> =
                full.pairs.iter().filter(|p| p.open_line >= b).copied().collect();
            assert_eq!(seg.pairs, expect_pairs, "边界 {b} 起段扫描的配对应与整篇一致");
            let expect_bnd: Vec<usize> =
                full.boundaries.iter().copied().filter(|&x| x >= b).collect();
            assert_eq!(seg.boundaries, expect_bnd, "边界 {b} 起的边界列表应一致");
        }
    }

    // ------------------------------------------------------------------
    // 折叠候选纯函数（`scan_fold_candidates`）
    // ------------------------------------------------------------------

    #[test]
    fn fold_candidates_find_indented_blocks() {
        let lines = [
            "fn outer() {",
            "    fn inner() {",
            "        x();",
            "    }",
            "    y();",
            "}",
            "fn next() {}",
        ];
        let cands = scan_fold_candidates(line_fn(&lines), lines.len());
        // outer 块跨 0..5（藏 1..5），inner 块跨 1..3（藏 2）——两者并存（候选允许嵌套）。
        assert!(cands.contains(&Fold { start: 0, end: 5 }), "{cands:?}");
        assert!(cands.contains(&Fold { start: 1, end: 3 }), "{cands:?}");
        // 空行不干扰、无嵌套内容的单行块不产候选。
        let blank = ["fn a() {", "", "    x", "}"];
        assert_eq!(
            scan_fold_candidates(line_fn(&blank), blank.len()),
            vec![Fold { start: 0, end: 3 }]
        );
    }

    #[test]
    fn fold_candidates_identical_to_view_metric() {
        // 用 Tab 缩进回归：行首空白按**字节**计（`leading_indent(..).bytes()`，
        // 与历史 `view::leading_ws_len` 相同），保证候选集合与折叠展开/收拢语义
        // 不因口径漂移而变化。
        let lines = ["\tfn a() {", "\t\tx;", "\t}"];
        assert_eq!(
            scan_fold_candidates(line_fn(&lines), lines.len()),
            vec![Fold { start: 0, end: 2 }]
        );
    }

    // ------------------------------------------------------------------
    // §8 缩进引导线纯函数（`indent_guide_level` / `blank_guide_level`）
    // ------------------------------------------------------------------

    /// §8.1 主源级数：`level = (indent=0) ? 0 : ceil(cols/is)`。is=4 时 0/3/4/5/8/9 →
    /// 0/1/1/2/2/3；Tab 混合 units（空格=1、Tab=4）与纯空格同式（
    /// `leading_indent(" \tX").units()` = 1+4 = 5 → 2）。
    #[test]
    fn indent_guide_level_table() {
        use super::indent_guide_level;
        assert_eq!(indent_guide_level(0, 4), 0);
        assert_eq!(indent_guide_level(3, 4), 1);
        assert_eq!(indent_guide_level(4, 4), 1);
        assert_eq!(indent_guide_level(5, 4), 2);
        assert_eq!(indent_guide_level(8, 4), 2);
        assert_eq!(indent_guide_level(9, 4), 3);
        // Tab 混合前导空白：units = 1空格 + 1Tab×4 = 5 → ceil(5/4)=2。
        assert_eq!(indent_guide_level(leading_indent(" \tX").units(), 4), 2);
        // is 非常量（2）仍按 ceil。
        assert_eq!(indent_guide_level(5, 2), 3);
    }

    /// §8.1 空行续线级数：A/B 任一缺 → 0；A<B → 1+⌊A/is⌋；A==B → ⌈B/is⌉；A>B → 1+⌊B/is⌋。
    #[test]
    fn blank_guide_level_four_branches() {
        use super::blank_guide_level;
        // A/B 任一缺 → 0（首行空行 / 文档尾空行）。
        assert_eq!(blank_guide_level(None, Some(8), 4), 0);
        assert_eq!(blank_guide_level(Some(4), None, 4), 0);
        assert_eq!(blank_guide_level(None, None, 4), 0);
        // A<B → 1+⌊A/is⌋（§8.2 空行 L12：A=0,B=8 → 1；A=4,B=8 → 2）。
        assert_eq!(blank_guide_level(Some(0), Some(8), 4), 1);
        assert_eq!(blank_guide_level(Some(3), Some(8), 4), 1);
        assert_eq!(blank_guide_level(Some(4), Some(8), 4), 2);
        // A==B → ⌈B/is⌉（均等缩进 → 按本级续；均 0 → 0 无线）。
        assert_eq!(blank_guide_level(Some(4), Some(4), 4), 1);
        assert_eq!(blank_guide_level(Some(8), Some(8), 4), 2);
        assert_eq!(blank_guide_level(Some(0), Some(0), 4), 0);
        // A>B → 1+⌊B/is⌋。
        assert_eq!(blank_guide_level(Some(8), Some(4), 4), 2);
        assert_eq!(blank_guide_level(Some(8), Some(0), 4), 1);
    }

    // ------------------------------------------------------------------
    // 段扫描断点 + 增量重算 × 整篇扫描对拍（Wave2-S1）
    // ------------------------------------------------------------------

    /// 从某个「栈清空边界」以全新状态续扫，必须与从 0 整篇扫描在 `≥ 该行` 的
    /// fold 候选 / 边界上逐位一致——这是增量重算正确性的根基。
    #[test]
    fn segment_from_boundary_equals_full_from_that_line() {
        let lines = [
            "fn a() {",
            "    a1();",
            "}",
            "fn b() {",
            "    fn c() {",
            "        x();",
            "    }",
            "    b1();",
            "}",
            "fn d() {}",
            "fn e() {",
            "    y();",
            "}",
        ];
        let full = scan_fold_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        assert!(full.boundaries.len() >= 2, "文本应产生多个边界 {full:?}");
        for &b in &full.boundaries {
            let seg = scan_fold_segment(line_fn(&lines), lines.len(), b, None, None, &[]);
            let expect_folds: Vec<Fold> =
                full.folds.iter().filter(|f| f.start >= b).copied().collect();
            assert_eq!(
                seg.folds, expect_folds,
                "边界 {b} 起段扫描的 fold 应与整篇一致"
            );
            let expect_bnd: Vec<usize> =
                full.boundaries.iter().copied().filter(|&x| x >= b).collect();
            assert_eq!(seg.boundaries, expect_bnd, "边界 {b} 起的边界列表应一致");
        }
    }

    /// 括号扫描的「栈空 + 不在块注释内」边界同理：从边界续扫 = 整篇在 `≥ 该行`
    /// 逐位一致（含块注释跨行状态）。
    #[test]
    fn bracket_segment_from_boundary_equals_full_from_that_line() {
        let lines = [
            "fn x() {",
            "    a();",
            "}",
            "/* { in comment", // 打开块注释
            "} */",            // 注释内闭括号应被忽略、不配对
            "fn y() {",
            "    b();",
            "}",
        ];
        let full = scan_bracket_segment(line_fn(&lines), lines.len(), 0, None, None, &[]);
        assert!(full.boundaries.len() >= 2, "应有多个括号边界 {full:?}");
        for &b in &full.boundaries {
            let seg = scan_bracket_segment(line_fn(&lines), lines.len(), b, None, None, &[]);
            let expect_pairs: Vec<BracketPair> =
                full.pairs.iter().filter(|p| p.open_line >= b).copied().collect();
            assert_eq!(seg.pairs, expect_pairs, "边界 {b} 起段扫描的配对应一致");
            // 字符表只保「边界行**之后**」：边界行 b 自身可能承载闭合「b 之前开口」的
            // 闭括号（fresh 重扫栈空 ⇒ 视为不匹配跳过，且行内开口深度依赖既有残差栈）——
            // 该行字符不可由重扫复现（由增量合并的 prev 保留承担，见 incremental 拆行）。
            let expect_chars: Vec<BracketChar> =
                full.chars.iter().filter(|c| c.line > b).copied().collect();
            assert_eq!(seg.chars, expect_chars, "边界 {b} 起段扫描的括号字符应一致");
            let expect_bnd: Vec<usize> =
                full.boundaries.iter().copied().filter(|&x| x >= b).collect();
            assert_eq!(seg.boundaries, expect_bnd, "边界 {b} 起的边界列表应一致");
        }
    }

    /// 把文本切成（ropey 语义）行访问闭包：`split('\n')` 保留尾换行的幻影空行，
    /// 与 `Buffer::line_count` / `Rope::lines_len` 口径一致；闭包捕获 `&str`（Copy）。
    fn text_line_fn(text: &str) -> impl Fn(usize) -> SharedString + Copy + '_ {
        move |i: usize| {
            text.split('\n')
                .nth(i)
                .map(SharedString::from)
                .unwrap_or_else(|| SharedString::new_static(""))
        }
    }

    /// 对拍核心：`prev`（上次落地结果，含快照网格）经一次「受影响行段描述
    /// (line_a,min_stop,delta)」的增量重算，五份输出必须与对该时点文本的**整篇扫描**
    /// 逐位一致（fold/配对/字符按 start / open_line / (line,col) 排序后比较——三者本就
    /// 是同一候选集合，仅内部次序约定不同）；快照网格随结果透传（供连续编辑续用）。
    fn assert_incremental_matches_full(
        prev: &(
            Vec<Fold>,
            Vec<BracketPair>,
            Vec<BracketChar>,
            Vec<usize>,
            Vec<usize>,
            Vec<(usize, FoldSnapshot)>,
            Vec<(usize, BracketSnapshot)>,
        ),
        text: &str,
        line_a: usize,
        min_stop: usize,
        delta: i64,
        multi: bool,
        full_limit: usize,
    ) -> (
        Vec<Fold>,
        Vec<BracketPair>,
        Vec<BracketChar>,
        Vec<usize>,
        Vec<usize>,
        Vec<(usize, FoldSnapshot)>,
        Vec<(usize, BracketSnapshot)>,
    ) {
        let line = text_line_fn(text);
        let n = text.split('\n').count();
        let (nf, np, nc, nfb, npb, nfs, nbs) = incremental_fold_brackets(
            line,
            n,
            full_limit,
            &prev.0,
            &prev.1,
            &prev.2,
            &prev.3,
            &prev.4,
            &prev.5,
            &prev.6,
            Some((line_a, min_stop, delta)),
            multi,
        );
        let (ef, ep, ec, efb, epb, ..) = full_fold_brackets(line, n);
        let mut nf_sorted = nf.clone();
        nf_sorted.sort_by_key(|f| f.start);
        let mut ef_sorted = ef.clone();
        ef_sorted.sort_by_key(|f| f.start);
        assert_eq!(nf_sorted, ef_sorted, "fold 候选与整篇扫描不一致（text={text:?}）");
        let mut np_sorted = np.clone();
        np_sorted.sort_by_key(|p| p.open_line);
        let mut ep_sorted = ep.clone();
        ep_sorted.sort_by_key(|p| p.open_line);
        assert_eq!(np_sorted, ep_sorted, "括号配对与整篇扫描不一致（text={text:?}）");
        let mut nc_sorted = nc.clone();
        nc_sorted.sort_by_key(|c| (c.line, c.col));
        let mut ec_sorted = ec.clone();
        ec_sorted.sort_by_key(|c| (c.line, c.col));
        assert_eq!(nc_sorted, ec_sorted, "括号字符与整篇扫描不一致（text={text:?}）");
        assert_eq!(nfb, efb, "fold 边界与整篇扫描不一致（text={text:?}）");
        assert_eq!(npb, epb, "括号边界与整篇扫描不一致（text={text:?}）");
        (nf, np, nc, nfb, npb, nfs, nbs)
    }

    /// **快照起点「跨包络平移」括对的 `guide_body_min_units` 重算回归**（预存字段新增，
    /// K160 对拍）：`bb` 为快照起点时，跨包络项（`open < bb`、`close > stop_old`）由旧值
    /// 平移保留——其体内行含受影响段、内容已受编辑 → 新字段旧值陈旧，必须用**新文档**
    /// 重算，否则与整篇扫描逐位不一致（本测试钉死这条路径）。
    ///
    /// 构造：行 0 永久栈底 `{`（直到 1102 才闭 → 全部行无空栈边界，逼起点走快照）；
    /// 行 500 层-4 `{` 开口、行 1101 层-4 `}` 闭合（跨行括对）；行 521 埋锚（缩进 8）。
    /// 编辑 = 行 521 锚行内容 `xxMARKxx();` → `xxMARKxx() { }`（行内自平衡，delta=0）：
    /// 起点 `pick_start(521)` 无边界可用 → 取快照 512（`bb_is_snapshot`）；因为括号自平衡、
    /// 新态在 1024（下一个快照检查点）与旧态一致 → 重扫在 1024 停 → 括对 (500,1101) 的
    /// `close=1101 > stop_old=1024`、`open=500 < 1024` = **跨包络平移项**，且体内行 521 缩进
    /// 由 8→4 改变了最小非闭缩进（8→4）——若不平移处重算，增量 8 ≠ 整篇 4，对拍必红。
    #[test]
    fn snapshot_crossing_pair_guide_body_min_units_recomputed() {
        fn fixture() -> String {
            let mut s = String::new();
            s.push_str("{\n"); // 0：永久栈底（未闭直到 1102 → 无空栈边界）
            for _ in 1..500 {
                s.push_str("  filler();\n");
            }
            s.push_str("    fn g() {\n"); // 500：层-4 开口
            for i in 501..1101 {
                if i == 521 {
                    s.push_str("        xxMARKxx();\n"); // 521：锚（缩进 8，与体内一致）
                } else {
                    s.push_str("        filler();\n");
                }
            }
            s.push_str("    }\n"); // 1101：层-4 闭（(500,1101) 括对）
            s.push_str("}\n"); // 1102：层-0 闭（(0,1102) 括对 + 边界）
            s
        }
        let old = fixture();
        let n = old.split('\n').count();
        let line = text_line_fn(&old);
        let prev = full_fold_brackets(line, n);
        // 前置：正是「栈永不清空」型（无空栈边界 ≤ 编辑行）、含跨行括对。
        assert!(
            prev.4.iter().all(|&b| b > 521),
            "前置：编辑行前不应有空栈边界（bb 必须走快照），boundaries={:?}",
            prev.4
        );
        assert!(
            prev.1.iter().any(|p| p.open_line == 500 && p.close_line == 1101),
            "前置：应有层-4 尾段跨行括对 (500,1101)"
        );
        // 旧值：体内最小非闭缩进本为 8（锚行缩进 8；`    fn g() {` / `}` 的 500/1101 不在体内）。
        let big = prev.1.iter().find(|p| p.open_line == 500).unwrap();
        assert_eq!(big.guide_body_min_units, Some(8), "旧值应为 8");

        // 编辑：锚行内容改缩进 8→4 且行内自平衡（净行数增量 0）。
        let mut new = old.clone();
        let pos = new.find("        xxMARKxx();").unwrap();
        new.replace_range(
            pos..pos + "        xxMARKxx();".len(),
            "    xxMARKxx() { }",
        );
        // 行 521 内容替换（无换行）→ delta=0、min_stop=522。
        let line_a = new[..pos].matches('\n').count();
        assert_eq!(line_a, 521, "锚行 0idx 应为 521，实际 {line_a}");
        let state = assert_incremental_matches_full(&prev, &new, 521, 522, 0, false, usize::MAX);
        // 显式钉新值：跨包络平移的 (500,1101) 重算后 = 体内最小缩进 4（锚行由 8 降 4）。
        let np = state.1;
        let crossing = np.iter().find(|p| p.open_line == 500 && p.close_line == 1101);
        assert_eq!(
            crossing.map(|p| p.guide_body_min_units),
            Some(Some(4)),
            "跨包络平移的括对须用新文档重算（8→4）：{crossing:?}"
        );
    }

    /// 结构编辑序列上的对拍：每一步的增量重算输出 == 该时点整篇扫描（fold 候选 /
    /// 括号配对 / 括号字符 / 边界五份全比），覆盖行内插换行、多行删除、注释内加花括号、
    /// 整函数结构重写、删整个内层块等编辑形态。
    #[test]
    fn incremental_edits_match_full_scan() {
        let src = "\
const S: &str = \"{ not paren\";
fn outer() {
    let x = {
        inner();
        more();
    };
    /* block { comment */
    z();
    // tail }
    {
        b1();
    }
    end();
}
fn after() {
    tail();
}";
        // 上次落地结果（取整篇扫描作为对拍基）。
        let line = text_line_fn(src);
        let n = src.split('\n').count();
        let prev = full_fold_brackets(line, n);
        let mut text = src.to_string();
        let mut state = prev;

        // (被替换的旧片段, 新片段, multi)
        let edits: Vec<(&str, &str, bool)> = vec![
            ("more();", "more();\n        deep();\n        deeper();", false), // 行内插 2 换行（净 +2，嵌套变深）
            ("        inner();\n", "", false),                                // 多行删除（净 −1）
            ("/* block { comment */", "/* block { extra { comment */", false), // 注释内加花括号（括号扫描忽略）
            (
                "fn after() {\n    tail();\n}",
                "fn after() {\n    if okay {\n        tail();\n    }\n}\nfn extra() {",
                false,
            ), // 整函数结构重写（净 +3）
            ("    {\n        b1();\n    }\n", "", false),                      // 删整个内层块（净 −3）
            ("    end();", "    end(); // { },", false),                       // 同行加括号字符（行数不变）
            ("ignore-me-2", "ignored", true), // multi → 整篇兜底（预期仍与整篇一致）
            ("fn outer() {\n    let x", "fn outer() {\n    fn branch() {\n        q();\n    }\n    let x", false), // 段首插入新函数（净 +2）
        ];
        for (i, (old_piece, new_piece, multi)) in edits.iter().enumerate() {
            if *old_piece == "ignore-me-2" {
                // multi 兜底：编辑描述不参与（引擎退整篇），这里仅验证结果=整篇。
                state = assert_incremental_matches_full(
                    &state, &text, 0, text.split('\n').count(), 0, true, usize::MAX,
                );
                continue;
            }
            let pos = text.find(old_piece).unwrap_or_else(|| panic!("edit {i}: 锚点 {old_piece:?} 未找到"));
            let end = pos + old_piece.len();
            let line_a = text[..pos].matches('\n').count();
            let line_b = text[..end].matches('\n').count();
            let insert_rows = new_piece.matches('\n').count() + 1;
            let removed_rows = line_b.saturating_sub(line_a) + 1;
            let min_stop = line_a + insert_rows;
            let delta = insert_rows as i64 - removed_rows as i64;
            text.replace_range(pos..end, new_piece);
            state = assert_incremental_matches_full(
                &state, &text, line_a, min_stop, delta, *multi, usize::MAX,
            );
        }
    }

    /// 边界行残差闭号增量对拍（2026-11-14 修复回归，K160）：编辑**恰落在边界行**
    /// （`bb == line_a`，如 `fn f() {\n    x();\n}` 的 `}` 行）时，该行承担的「闭合
    /// bb 之前残差开口」的 `}` 字符从 bb 以空栈重扫**造不出**（栈空视为不匹配跳过，
    /// 见 _incremental_fold_brackets 注释的边界特殊性）→ 必须回退到前一边界重扫，
    /// 增量五份输出方与整篇扫描逐位一致（否则该残差 `}` 及其字符表项丢失）。
    /// 此处把 line 2 的 `}` 编辑为 `} // tail`（净 0 行、行号不动），编辑点恰在边界行。
    #[test]
    fn incremental_on_boundary_line_keeps_residual_close_char() {
        let src = "fn f() {\n    x();\n}\nfn g() {\n    y();\n}";
        let line = text_line_fn(src);
        let n = src.split('\n').count();
        let prev = full_fold_brackets(line, n);
        assert!(
            prev.4.contains(&2),
            "fixture 的行 2（`}}`）应是括号边界: {:?}",
            prev.4
        );
        // 编辑恰在边界行 line_a==bb==2：`}` → `} // tail`（净 0 行）。
        assert_incremental_matches_full(
            &prev,
            "fn f() {\n    x();\n} // tail\nfn g() {\n    y();\n}",
            2, // line_a
            3, // min_stop
            0, // delta
            false,
            usize::MAX,
        );
    }

    // ------------------------------------------------------------------
    // 打字卡顿（⑦）：栈永不清空文档的增量包络退化回归（TDD 先红后绿）
    // ------------------------------------------------------------------

    /// 深嵌套「栈永不清空」夹具（打字卡顿 ⑦）：首行缩进 0 的未闭 `{` 作**永久栈底**——
    /// 之后所有行缩进 ≥1 且不以闭记号开头 → fold 栈底 `(0,0)` 永不被弹、括号层-0 栈
    /// 永非空 → 两者「栈空边界」全部消失（退化用例：旧实现每次敲括号 0..EOF 整篇重扫，
    /// 20k 深嵌套 debug 85-135ms）。行 `anchor_row` 埋唯一锚 `// [[TA]]` 供测试定位编辑点
    /// （内存构造、禁写盘）。
    fn pathological_fixture(total: usize, anchor_row: usize) -> String {
        let mut s = String::with_capacity(total * 40);
        s.push_str("{\n"); // 行 0：缩进 0 未闭 → 永久栈底
        for i in 1..total {
            if i == anchor_row {
                s.push_str("  c(); // [[TA]]\n");
                continue;
            }
            match i % 6 {
                1 => s.push_str("  a();\n"),                 // 缩进 2
                2 => s.push_str("  {\n"),                    // 缩进 2 块开（跨行括对）
                3 => s.push_str("    b();\n"),               // 缩进 4
                4 => s.push_str("  }\n"),                    // 缩进 2 块闭（跨行括对）
                5 => s.push_str("  c();\n"),                 // 缩进 2
                _ => s.push_str("  fn g() { d(); }\n"),      // 行内平衡（不产跨行括对）
            }
        }
        s
    }

    /// 单次行内编辑的增量对拍 + 包络探针（打字卡顿 ⑦）：把 `old` 中的锚位
    /// `// [[TA]]`（`old` 坐标 `pos`）替换为 `piece`（含新括号、无换行 → 行数不变、
    /// delta=0）。返回 `(fold_span, bracket_span, 五份逐字节一致)`。
    /// span = 本次增量实际重扫包络 `stop - from`（**确定性结构量**，禁纯墙钟）。
    fn probe_envelope(
        prev: &(
            Vec<Fold>,
            Vec<BracketPair>,
            Vec<BracketChar>,
            Vec<usize>,
            Vec<usize>,
            Vec<(usize, FoldSnapshot)>,
            Vec<(usize, BracketSnapshot)>,
        ),
        old: &str,
        piece: &str,
        pos: usize,
    ) -> (usize, usize, bool) {
        let mut new = old.to_string();
        new.replace_range(pos..pos + "// [[TA]]".len(), piece);
        let line_a = old[..pos].matches('\n').count();
        let min_stop = line_a + 1; // 行内替换（无换行）：受影响段 = 编辑行下一行
        let delta = 0i64;
        let (nf, np, nc, nfb, npb, _nfs, _nbs) = incremental_fold_brackets(
            text_line_fn(&new),
            new.split('\n').count(),
            usize::MAX,
            &prev.0,
            &prev.1,
            &prev.2,
            &prev.3,
            &prev.4,
            &prev.5,
            &prev.6,
            Some((line_a, min_stop, delta)),
            false,
        );
        let [ff, fstop, bf, bstop] = last_incremental_spans();
        let (ef, ep, ec, efb, epb, _efs, _ebs) =
            full_fold_brackets(text_line_fn(&new), new.split('\n').count());
        let mut nf2 = nf.clone();
        nf2.sort_by_key(|f| f.start);
        let mut ef2 = ef.clone();
        ef2.sort_by_key(|f| f.start);
        let mut np2 = np.clone();
        np2.sort_by_key(|p| p.open_line);
        let mut ep2 = ep.clone();
        ep2.sort_by_key(|p| p.open_line);
        let mut nc2 = nc.clone();
        let mut ec2 = ec.clone();
        nc2.sort_by_key(|c| (c.line, c.col));
        ec2.sort_by_key(|c| (c.line, c.col));
        let exact = nf2 == ef2 && np2 == ep2 && nc2 == ec2 && nfb == efb && npb == epb;
        (fstop.saturating_sub(ff), bstop.saturating_sub(bf), exact)
    }

    /// 打字卡顿回归（⑦）：深嵌套「栈永不清空」文档上，结构编辑的增量重扫包络必须**有界**
    /// —— 稳定编辑（打字常态：行内插入自平衡 `{ }`）应收成小块（≲ 档距量级，杜绝退化
    /// EOF）；破坏性编辑（打字瞬态：行内插入未闭 `{` → 影响面顺移至 EOF）至少受起点断点
    /// 约束、包络 < 总行数（不再 0..EOF）。两例增量五份输出均须与整篇扫描**逐字节一致**
    /// （K160：不许用正确性换速度）。TDD：先红（旧实现包络 0..EOF 双双败）后绿。
    #[test]
    fn incremental_envelope_bounded_when_stack_never_empties() {
        const TOTAL: usize = 12_001;
        const ANCHOR: usize = 6_000;
        let old = pathological_fixture(TOTAL, ANCHOR);
        let n = old.split('\n').count();
        assert!(n > 11_000, "夹具应 ≈8-20k 行量级：n={n}");
        // 前置（病理）：至少一侧空栈边界退化（代表「包络退化」场景），否则用例不相关。
        let prev = full_fold_brackets(text_line_fn(&old), n);
        assert!(
            prev.3.is_empty() || prev.4.is_empty(),
            "病理夹具应使空栈边界退化（fb={} pb={}）",
            prev.3.len(),
            prev.4.len()
        );
        let pos = old.find("// [[TA]]").unwrap();

        // —— 稳定编辑（打字常态：行内插入自平衡 `{ }`）——
        let (fs, bs, exact) = probe_envelope(&prev, &old, "{ } // [[TA]]", pos);
        assert!(exact, "稳定编辑增量五份输出必须与整篇扫描逐字节一致（K160）");
        assert!(
            fs < n / 20 && bs < n / 20,
            "稳定编辑包络应收成小块（打字不整篇重扫）：fold={fs}/{n} bracket={bs}/{n}"
        );

        // —— 破坏性编辑（打字瞬态：行内插入未闭 `{`）——
        let (fs2, bs2, exact2) = probe_envelope(&prev, &old, "{ // [[TA]]", pos);
        assert!(exact2, "破坏性编辑增量五份输出必须与整篇扫描逐字节一致（K160）");
        assert!(
            fs2 < n && bs2 < n,
            "破坏性编辑包络受起点断点约束，不得退化 0..EOF：fold={fs2}/{n} bracket={bs2}/{n}"
        );
    }

    /// 深嵌套「栈永不清空」夹具上与 `bench_full_vs_incremental_scan` 平行的**打字耗时旁证**
    /// （仅打印、不落断言；`#[ignore]`，本机定位用）。先量「old 基线 = 整篇双扫」（旧实现
    /// 在该文档等效于每次整篇重扫），再量修复后同一稳定编辑的**增量**重扫耗时，目标是量级
    /// 级下降（>10×）。跑法：`cargo test --release --bin vuick fold::bench_deep_nest_typing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_deep_nest_typing() {
        const TOTAL: usize = 20_001;
        const ANCHOR: usize = 10_000;
        let old = pathological_fixture(TOTAL, ANCHOR);
        // 与生产同款 ropey Rope + line_shared（O(log n)/行；不用 text_line_fn——
        // `split('\n').nth(i)` 是 O(i)/行 → O(n²)，会虚高两侧耗时）。
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from_str(&old);
        let n = rope.lines_len();
        let line = |r: usize| crate::editor::buffer::line_shared(&rope, r);
        let prev = full_fold_brackets(line, n);
        // 基线 = 整篇双扫（旧实现增量在该夹具退化为此量级）。
        let mut t_full = Vec::new();
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            let r = full_fold_brackets(line, n);
            std::hint::black_box(&r);
            t_full.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        // 稳定编辑：行内插入自平衡 `{ }`（编辑点 = 锚行 ANCHOR，行数不变、delta=0）。
        let new_text = old.replacen("// [[TA]]", "{ } // [[TA]]", 1);
        let line_a = ANCHOR; // 锚行（0-based 编辑行，旧坐标）
        let new_rope = ropey::Rope::from_str(&new_text);
        let new_line = |r: usize| crate::editor::buffer::line_shared(&new_rope, r);
        let mut t_inc = Vec::new();
        for _ in 0..7 {
            let t0 = std::time::Instant::now();
            let r = incremental_fold_brackets(
                new_line,
                new_rope.lines_len(),
                usize::MAX,
                &prev.0,
                &prev.1,
                &prev.2,
                &prev.3,
                &prev.4,
                &prev.5,
                &prev.6,
                Some((line_a, line_a + 1, 0)),
                false,
            );
            std::hint::black_box(&r);
            t_inc.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        t_full.sort_by(|a, b| a.partial_cmp(b).unwrap());
        t_inc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med_full = t_full[t_full.len() / 2];
        let med_inc = t_inc[t_inc.len() / 2];
        eprintln!(
            "[BENCH ⑦] rows={n} 稳定编辑增量包络=[{:?}]（总行 {n}，包络/总行= {:.2}）",
            last_incremental_spans(),
            (last_incremental_spans()[1] - last_incremental_spans()[0]) as f64 / n as f64,
        );
        eprintln!(
            "[BENCH ⑦] before(整篇双扫=旧增量退化量级) med: {:.2} ms | after(增量) med: {:.2} ms | 加速 {:.0}x",
            med_full, med_inc, med_full / med_inc.max(1e-6),
        );
    }

    /// 安全护栏（K142）：单行超预算密括号（1 行 > `BRACKET_CHAR_MAX` 个括字符，row_count=1
    /// 可绕开 limits.rs 200k 行守卫）不得 OOM / 无界增长——扫描必须停在一处上界内、置
    /// `truncated` 且不 panic；上层 `full_fold_brackets` 返回空括对/字符/括对边界表（该文件
    /// 整体不渲染括对/高亮，视觉降级可接受），折叠候选不受影响。纯内存构造、无文件 IO。
    #[test]
    fn dense_bracket_input_bounded() {
        let opens = "(".repeat(BRACKET_CHAR_MAX + 1000);
        let closes = ")".repeat(BRACKET_CHAR_MAX + 1000);
        let lines: &[&str] = &[opens.as_str(), closes.as_str()];
        let lf = line_fn(lines);
        let (folds, pairs, chars, fb, pb, _fs, _bs) = full_fold_brackets(lf, lines.len());
        assert!(pairs.is_empty(), "截断后配对应为空: len={}", pairs.len());
        assert!(chars.len() <= BRACKET_CHAR_MAX, "chars 必须 ≤ 预算: {}", chars.len());
        assert!(pb.is_empty(), "截断后括对边界应为空: {:?}", pb);
        assert!(!folds.is_empty() || !fb.is_empty(), "折叠候选/边界不受括对截断影响");
        // 报告级：直接扫描也置 truncated；截断中途不产出任何配对。
        let report = scan_bracket_segment(lf, lines.len(), 0, None, None, &[]);
        assert!(report.truncated, "超预算输入必须置 truncated");
        assert!(report.chars.len() <= BRACKET_CHAR_MAX);
        assert!(report.pairs.is_empty(), "截断中途的配对同样不应流出");
        // 单行 `{`（缩进 0，花括号栈同层爆长度）同样受未闭预算约束。
        let braces = "{".repeat(BRACKET_CHAR_MAX + 1000);
        let blines: &[&str] = &[braces.as_str()];
        let blf = line_fn(blines);
        let breport = scan_bracket_segment(blf, blines.len(), 0, None, None, &[]);
        assert!(breport.truncated, "单行密 `{{` 也应置 truncated");
        assert!(breport.chars.len() <= BRACKET_CHAR_MAX);
    }

    /// 增量前提缺失的兜底路径：`multi` / 无编辑描述 → 整篇扫描；超行数阈值 →
    /// 候选/配对/边界全部清空（按需折叠语义）。
    #[test]
    fn incremental_falls_back_to_full_or_by_need() {
        let src = "fn a() {\n    x();\n}\n";
        let line = text_line_fn(src);
        let prev = full_fold_brackets(line, src.split('\n').count());
        // multi=true → 整篇兜底（输出=整篇含快照，边界更新）。
        let r = incremental_fold_brackets(
            line, src.split('\n').count(), usize::MAX,
            &prev.0, &prev.1, &prev.2, &prev.3, &prev.4, &prev.5, &prev.6, Some((0, 4, 0)), true,
        );
        let full = full_fold_brackets(line, src.split('\n').count());
        assert_eq!(r.0, full.0);
        assert_eq!(r.1, full.1);
        assert_eq!(r.2, full.2);
        assert_eq!(r.3, full.3);
        assert_eq!(r.4, full.4);
        assert_eq!(r.5, full.5, "multi 兜底的 fold 快照网格应=整篇");
        assert_eq!(r.6, full.6, "multi 兜底的括号快照网格应=整篇");
        // edit=None → 整篇兜底。
        let r2 = incremental_fold_brackets(
            line, src.split('\n').count(), usize::MAX,
            &prev.0, &prev.1, &prev.2, &prev.3, &prev.4, &prev.5, &prev.6, None, false,
        );
        assert_eq!(r2.0, full.0);
        // row_count > full_limit → 全部清空（按需折叠/配对/字符/快照）。
        let r3 = incremental_fold_brackets(
            line, src.split('\n').count(), 2,
            &prev.0, &prev.1, &prev.2, &prev.3, &prev.4, &prev.5, &prev.6, Some((0, 4, 0)), false,
        );
        assert!(
            r3.0.is_empty()
                && r3.1.is_empty()
                && r3.2.is_empty()
                && r3.3.is_empty()
                && r3.4.is_empty()
                && r3.5.is_empty()
                && r3.6.is_empty()
        );
    }

    // ------------------------------------------------------------------
    // 性能基准（含 release 跑法）：整篇双扫（旧，每次结构编辑 O(全文)）vs 增量
    // （新，受影响段局部重扫 + 尾部平移）。
    // ------------------------------------------------------------------

    /// 合成大文档：N 个函数体（含嵌套块），模拟 16 万行级别源码结构。
    fn large_src(funcs: usize) -> String {
        let mut s = String::with_capacity(funcs * 48);
        for i in 0..funcs {
            s.push_str(&format!("fn f{i}() {{\n    let x = {{\n        inner_{i}();\n    }};\n    z_{i}();\n}}\n"));
        }
        s
    }

    /// release 跑法：`cargo test --release --bin vuick fold:: -- --ignored --nocapture`。
    /// 测量同一份 16 万行文档上：
    /// - `before` = 整篇双扫（fold 候选 + 括号配对，即旧实现的每次结构编辑代价）；
    /// - `after`  = 文档中部一次结构编辑的**增量**重算（受影响段局部重扫 + 尾部平移）。
    #[test]
    #[ignore]
    fn bench_full_vs_incremental_scan() {
        let lines_target = 160_000usize;
        let funcs = lines_target / 6 + 10; // 每个函数 6 行（含最后一个换行的幻影空行）
        let src = large_src(funcs);
        let text: &str = &src;
        let line = text_line_fn(text);
        let n = text.split('\n').count();
        assert!(n >= lines_target, "n={n}");

        // 旧：整篇双扫（做 3 次取中位，模拟 ≥3 跑）。
        let mut t_full = Vec::new();
        for _ in 0..3 {
            let t0 = std::time::Instant::now();
            let (fl, pa, ..) = full_fold_brackets(line, n);
            std::hint::black_box(&fl);
            std::hint::black_box(&pa);
            t_full.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        // 以第一次全扫结果作为增量对拍基 + 基线。
        let (prev_folds, prev_pairs, prev_chars, prev_fb, prev_pb, prev_fsnaps, prev_bsnaps) =
            full_fold_brackets(line, n);

        // 新：文档中部一次结构编辑（函数 k 体内插入 2 行更深缩进 → 净 +2）的增量重算。
        let k = funcs / 2; // 中段
        let line_a = k * 6 + 2;
        let min_stop = line_a + 3;
        let delta = 2i64;
        let mut t_inc = Vec::new();
        for _ in 0..7 {
            let t0 = std::time::Instant::now();
            let (f, b, c, fb, pb, _fs, _bs) = incremental_fold_brackets(
                line, n + delta as usize, usize::MAX,
                &prev_folds, &prev_pairs, &prev_chars, &prev_fb, &prev_pb,
                &prev_fsnaps, &prev_bsnaps,
                Some((line_a, min_stop, delta)), false,
            );
            std::hint::black_box(&f);
            std::hint::black_box(&b);
            std::hint::black_box(&c);
            std::hint::black_box(&fb);
            std::hint::black_box(&pb);
            t_inc.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        t_full.sort_by(|a, b| a.partial_cmp(b).unwrap());
        t_inc.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med_full = t_full[t_full.len() / 2];
        let med_inc = t_inc[t_inc.len() / 2];
        eprintln!(
            "[BENCH fold/bracket] rows={n} folds={} pairs={} boundaries={}/{}",
            prev_folds.len(),
            prev_pairs.len(),
            prev_fb.len(),
            prev_pb.len()
        );
        eprintln!(
            "[BENCH] before(整篇双扫) min/med: {:.2}/{:.2} ms | after(增量) min/med: {:.2}/{:.2} ms | 加速 {:.0}x",
            t_full[0], med_full, t_inc[0], med_inc, med_full / med_inc.max(1e-6),
        );
    }

    /// 真文件诊断（`#[ignore]`，仅本机）：直接测量「P3 之前 EOF 打开路径在 **UI 线程**
    /// 同步整篇折叠候选＋括号配对重扫」在真实 `大文档测试.txt`（15.6 万行 / 6.1MB）上的
    /// 耗时——这是用户报告「打开会卡」的主线程卡顿量级（debug 下更大，release 同理更小
    /// 但相对量级不变）。P3 起该段工作改由 `refresh_fold_brackets` 在后台执行，UI 线程只
    /// 留 O(1) 换装；此处数字 = 已从首帧临界路径移出的同步开销。
    /// 跑法：`cargo test --release -p editor fold::bench_real_file_sync_scan -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real_file_sync_scan() {
        let path = r"E:\Rust\gpui-editor\target\大文档测试.txt";
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP: missing {path} ({e})");
                return;
            }
        };
        // 与生产 `recompute_fold_brackets_full` 的闭包同款：ropey Rope + line_shared
        // （O(log n)/行；不能用 text_line_fn——`split('\n').nth(i)` 是 O(i)/行 → O(n²)）。
        use crate::editor::rope_ext::RopeExt as _;
        let rope = ropey::Rope::from_str(&src);
        let n = rope.lines_len();
        assert!(n > 100_000, "真文件应有 >10 万行：{n}");
        let line = |r: usize| crate::editor::buffer::line_shared(&rope, r);
        let mut t = Vec::new();
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let (folds, pairs, chars, fb, pb, _fs, _bs) = full_fold_brackets(line, n);
            std::hint::black_box(&folds);
            std::hint::black_box(&pairs);
            std::hint::black_box(&chars);
            std::hint::black_box(&fb);
            std::hint::black_box(&pb);
            t.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        eprintln!(
            "[BENCH] real 大文档测试.txt: {n} 行 / {:.1} MB，EOF 同步折叠+括号全扫 min/med: {:.2}/{:.2} ms（P3 前在 UI 线程执行→即打开卡顿量级；P3 后移后台，UI 只 O(1) 换装）",
            src.len() as f64 / 1e6,
            t[0],
            t[t.len() / 2],
        );
    }
}
