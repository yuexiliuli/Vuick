//! 窗口式语法高亮：只对「可见行窗口（+前瞻回看）」跑 syntect 解析，并**增量缓存**。
//!
//! 设计吸收 **gpui-component/element.rs 的窗口式高亮**、本仓库 `src/syntax` 的
//! `WindowCache`，与 `markdown/wysiwyg` 的 `shape_rows`「只重排被编辑块」约定：
//! 解析量大但限定在可见窗口，滚动复用、编辑只重解析受影响邻域——
//! 大文档 `.rs` 逐键把整窗（~108 行过扫）重跑 syntect 就是 ~127ms/键卡顿的主因。
//!
//! 增量模型（syntect `ParseState` 实现 `Clone`，即官方文档指明的缓存机制）：
//! - 缓存**已解析窗口** `[base, base + lines.len())`：逐行样式 + 每行解析状态保存点
//!   （`saves[i]` = 解析完 `base+i` 行后的状态）。
//! - **命中**：请求窗口落在缓存内直接子切片返回（滚动零解析）。
//! - **向下扩展**（滚动下移）：从最后保存点续解析，只付新暴露行。
//! - **编辑增量**：`set_dirty((min_row, tail_new, delta))` 登记最近编辑；下次版本变更
//!   从 `min_row - lookback` 恢复保存点重解析，并在「已越过编辑新跨（r+1 ≥ tail_new）
//!   且本行样式 + 解析状态与旧缓存位移对应行一致」处提前终止——确定性地证明下方未变
//!   （同解析器 + 相同后续行 → 输出逐行确定），把逐键的重解析从整窗降为
//!   `lookback + 编辑跨` 量级（典型打字 ~几行）。
//!
//! 收敛判据是**充分且必要安全**的：仅在 `r+1 ≥ tail_new`（编辑新文本跨已全部重解析、
//! 下方全是内容未变的「尾部」行）时允许提前退出；未达标前任何样式巧合命中都不会
//! 早退，避免把编辑区间内的新行错当成未变而漏掉。`base` 之下、滚动上移等无法
//! 增量的情况原本回落到整段重解析（与旧实现一致，只影响该路径的代价、不影响正确性）。
//!
//! ![回跳快照层] 在上述「单一主窗口 + savepoints」之上再加一层**有界 LRU 快照缓存**：
//! 主窗口每次被丢弃/收缩（容量前移 drain、整段重锚）前，先把整窗镜像（`base` + `lines`
//! + `saves` + 版本 + 文档 id + 明暗）沉降为快照；回跳/回看（`lo < base`）时若命中最近
//! 解析过的区域快照，直接从其中**半程续栈**（快照覆盖到请求窗底 → 整窗复用，零 syntect；
//! 只覆盖一半 → 从快照最后保存点续解析，只付超出部分）重建主窗口，不再从空 `ParseState`
//! 整段重解析——「打开→下滚→回看→再下滚」的典型往返不再整窗重解析。快照语义与主窗口
//! 同款（`saves.len() == lines.len()` 不变量同样成立），只做「与主窗口同版本相同的解析」；
//! 编辑/换 Buffer/切明暗 → 全部快照失效；任何不确定一律保守回落到整段重解析（宁慢勿错）。
//!
//! 与上层解耦：产出是**与主题无关的语义名**再映射为 gpui `Hsla`（GitHub 明/暗两套
//! 内嵌色板），不依赖 gpui-component 的 `HighlightTheme`。

use std::{
    collections::HashMap,
    ops::Range,
    sync::{LazyLock, Mutex},
};

use gpui::{Hsla, rgb};
use syntect::parsing::{ParseState, Scope, ScopeStackOp, SyntaxReference, SyntaxSet};

use crate::editor::buffer::Buffer;

/// 单帧**同步**逐行扩展（`extend_down`）的行数预算（≈ 1.5× 典型视口行数）：滚动缺口
/// （目标窗口底 − 缓存上界 + 1）不超过此值时允许 UI 线程同步逐行续解析——滚轮/惯性微跳
/// （每帧几~几十行）保持即时上色、作用域栈完整；超过预算（中速拖动/惯性滚，缺口
/// 100-900 行）改走 [`Self::pending`] 延后 + settle **有界重锚**（`full_reparse`，
/// O(窗口+回看)≈一屏）：拖动帧只付空样式渲染（帧廉价、0 syntect），余量交 settle 补齐，
/// 不再每帧把整个缺口逐行重跑——大 `.rs` ~63µs/行，600 行缺口 ≈ 40-100ms/跳（release）/
/// 百 ms 级（debug），是「中速拖动不跟手」的根因。**注意**：首屏（缓存为空）不走此延后
/// 路径——打开/首帧同步上色（见 [`Self::accept_deferred`]），否则大文档先出纯文本再补色。
const EXTEND_SYNC_BUDGET_ROWS: usize = 160;

/// 回跳快照缓存预算（取舍）：`HL_SNAPSHOT_CAP` = 最多保留的快照份数；
/// `HL_SNAPSHOT_MAX_ROWS` = 单份快照的行数上限。
///
/// 快照 = 历史解析窗口的完整镜像（base + lines + saves + 版本 + 文档 id + 明暗），
/// 供「回跳/回看」半程续栈——主窗口被 drain / 重锚前沉降，回跳命中快照即可从保存点续栈
/// 或整窗复用，不再从空 `ParseState` 整段重解析。**单窗口仍是主线**：快照只是回跳优化层，
/// 不取代主缓存的命中/`extend_down`/编辑增量路径（滚动前进热路径零快照开销）。
///
/// CAP 取 4（连带行数上限）：滚轮/回跳通常只回最近 1-2 个窗口，4 个已覆盖「打开→下滚→
/// 回看→再下滚」的典型往返，又不吞内存；行数上限兜底超大窗口（窗口 2k+ 行时单份快照即
/// 超预算 → 拒绝快照，保守回落既有整窗重解析，宁慢勿错）。每次存快照 O(窗口) 复制，只在
/// 「即将离开保护区」（drain / 重锚）时做，不随滚动频率放大。
const HL_SNAPSHOT_CAP: usize = 4;
/// 单份快照最大行数（见 [`HL_SNAPSHOT_CAP`] 的取舍注释）。
const HL_SNAPSHOT_MAX_ROWS: usize = 2048;

/// 共享语法集（默认语法集几 MB，进程级共享一次）。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// 语言名 → 语法解析结果缓存（`find_syntax_by_token` 全量扫描较慢）。
static SYNTAX_CACHE: LazyLock<Mutex<HashMap<String, Option<&'static SyntaxReference>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let lang = language.to_owned();
    if let Ok(cache) = SYNTAX_CACHE.lock()
        && let Some(r) = cache.get(&lang) {
            return *r;
        }
    let r = SYNTAX_SET
        .find_syntax_by_token(&lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(&lang));
    if let Ok(mut cache) = SYNTAX_CACHE.lock() {
        cache.insert(lang, r);
    }
    r
}

/// 测试探针：进程级共享语法集引用（供基准校准直接测纯 syntect parse 成本）。
#[cfg(test)]
pub(crate) fn syntax_set_ref() -> &'static SyntaxSet {
    &SYNTAX_SET
}

/// syntect scope → 语义名（与 `src/syntax` 的 GitHub 配色语义一致）。
fn semantic_name(scope: &Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    if scope.starts_with("comment") {
        Some("comment")
    } else if scope.starts_with("constant.character.escape") {
        Some("string.escape")
    } else if scope.starts_with("string") {
        Some("string")
    } else if scope.starts_with("constant.numeric") {
        Some("number")
    } else if scope.starts_with("constant.language.boolean") {
        Some("boolean")
    } else if scope.starts_with("keyword.operator") {
        Some("operator")
    } else if scope.starts_with("keyword") || scope.starts_with("storage") {
        Some("keyword")
    } else if scope.starts_with("entity.name.function") || scope.starts_with("support.function") {
        Some("function")
    } else if scope.starts_with("entity.name.type")
        || scope.starts_with("entity.name.class")
        || scope.starts_with("support.type")
    {
        Some("type")
    } else if scope.starts_with("entity.name.tag") {
        Some("tag")
    } else if scope.starts_with("entity.other.attribute-name") {
        Some("attribute")
    } else if scope.starts_with("variable.other.property")
        || scope.starts_with("support.constant.property")
    {
        Some("property")
    } else if scope.starts_with("variable") {
        Some("variable")
    } else if scope.starts_with("constant") {
        Some("constant")
    } else if scope.starts_with("punctuation") {
        Some("punctuation")
    } else if scope.starts_with("entity.name.namespace") || scope.starts_with("keyword.control.import")
    {
        Some("preproc")
    } else {
        None
    }
}

/// 语义名 → 配色（内嵌 GitHub Light/Dark 色板）。
fn name_color(name: &str, dark: bool) -> Option<Hsla> {
    let c: u32 = match (name, dark) {
        ("comment", false) => 0x6e7781,
        ("comment", true) => 0x8b949e,
        ("string", false) => 0x0a3069,
        ("string", true) => 0xa5d6ff,
        ("string.escape", false) => 0x116329,
        ("string.escape", true) => 0xff7b72,
        ("number", false) => 0x0550ae,
        ("number", true) => 0x79c0ff,
        ("boolean", false) => 0xcf222e,
        ("boolean", true) => 0xd2a8ff,
        ("operator", false) => 0xcf222e,
        ("operator", true) => 0xff7b72,
        ("keyword", false) => 0xcf222e,
        ("keyword", true) => 0xff7b72,
        ("function", false) => 0x8250df,
        ("function", true) => 0xd2a8ff,
        ("type", false) => 0x953800,
        ("type", true) => 0xffa657,
        ("tag", false) => 0x116329,
        ("tag", true) => 0x7ee787,
        ("attribute", false) => 0x116329,
        ("attribute", true) => 0x7ee787,
        ("property", false) => 0x116329,
        ("property", true) => 0x79c0ff,
        ("variable", false) => 0x24292f,
        ("variable", true) => 0xe6edf3,
        ("constant", false) => 0x0550ae,
        ("constant", true) => 0x79c0ff,
        ("punctuation", false) => 0x24292f,
        ("punctuation", true) => 0xe6edf3,
        ("preproc", false) => 0x116329,
        ("preproc", true) => 0xffa657,
        _ => return None,
    };
    Some(rgb(c).into())
}

/// 历史解析窗口快照：主窗口被丢弃/重锚前的一帧完整镜像（逐行样式 + 每行解析状态保存点）。
/// 语义与主缓存同款：`saves[i]` = 解析完 `base + i` 行后的状态，`saves.len() == lines.len()`
/// 不变量同样成立。`doc_id`/`version`/`dark` 记录建立时上下文，命中三要素都要对得上
/// （换文档/编辑换版本/切明暗 → 快照不得复用，宁慢勿错）。
struct HighlightSnapshot {
    /// 所属文档 id（与 Buffer::id 对照，杜绝「换 Buffer 后 version 归零」命中上一份文档）。
    doc_id: u64,
    /// 建立时的 buffer 版本（编辑 → 版本跳变 → 该快照整体过期）。
    version: u64,
    /// 解析窗口锚点（逻辑行，`lines[0]` = 第 `base` 行）。
    base: usize,
    /// 建立时的明暗色板（快照存的 `Hsla` 依赖 `dark`，切明暗后不得复用）。
    dark: bool,
    /// 逐行样式（相对每行文本字节）。
    lines: Vec<Vec<(Range<usize>, Hsla)>>,
    /// 每行解析状态保存点（`saves[i]` = 解析完 `base + i` 行后的状态）。
    saves: Vec<ParseState>,
    /// LRU 最近使用钟（淘汰序，cap 小 → 线性扫够用）。
    last_seen: u32,
}

/// 高亮器：持有语言语法 + 「增量解析缓存」。
pub struct Highlighter {
    syntax: Option<&'static SyntaxReference>,
    dark: bool,
    /// 缓存解析起点（逻辑行，含回看前缀）；`lines[0]` = 第 `base` 行。
    base: usize,
    /// 逐行样式（相对每行文本字节）。
    lines: Vec<Vec<(Range<usize>, Hsla)>>,
    /// 每行的 syntect 解析状态保存点（`saves[i]` = 解析完 `base + i` 行后的状态）。
    saves: Vec<ParseState>,
    /// 缓存建立时的 `buffer.version`；版本变 = 有编辑。
    gated_version: u64,
    /// 缓存建立时的文档 id（与 `gated_version` 一起判别「是否同一份文档」：换 Buffer 后
    /// version 可能归零，`gated_version` 撞车不能当作命中，须 `gated_id` 也一致——见
    /// [`Buffer::id`] 与 `invalidate` 的 version-0 冲突回归）。
    gated_id: u64,
    /// 回跳快照 LRU（见 [`HighlightSnapshot`] / [`HL_SNAPSHOT_CAP`]）：主窗口被 drain/重锚
    /// 前沉降当前窗口，回跳命中快照即半程续栈重建主窗口，不再从空 `ParseState` 整段重解析。
    snapshots: Vec<HighlightSnapshot>,
    /// 快照 LRU 访问钟（`HighlightSnapshot::last_seen`，淘汰最久未用）。
    snapshot_clock: u32,
    /// 待消费的「最近编辑」登记（由 `set_dirty` 写入，仅消费一次）：
    /// `(min_row, tail_new, delta)` —— `min_row` 编辑前最小受影响逻辑行；
    /// `tail_new` 编辑后在**当前坐标**下首个内容未变的「尾部」行
    /// （= `min_row + 新文本行数`）；`delta` 净行增量（新行数 − 被删行数）。
    dirty: Option<(usize, usize, i64)>,
    /// 语言/色板切换强制整窗重解析（跳过增量路径）。
    force_full: bool,
    /// 滚动（拖拽/快滚）期间被延后的高亮窗口 `(lo, hi, lookback)`：缺口超过单帧同步预算
    /// （中速拖动/惯性滚 100-900 行到暴跳）时，若每帧同步 `extend_down` 逐行扫整个缺口
    /// （DEBUG 下 syntect ~百 ms/帧）或 `full_reparse`，会把帧率压到个位数，滑块/内容
    /// 跟不紧鼠标。先登记 pending、本帧用空样式渲染（帧廉价），滚动停住（相邻帧窗口
    /// 一致）再由 `settle_pending` 补一次有界重锚（O(窗口+回看)≈一屏）。
    pending: Option<(usize, usize, usize)>,
    /// 测试探针（`cfg(test)`）：本实例 `full_reparse`（整段重解析建缓存）被调次数。
    /// P1a 渐进打开回归用它断言「追加块完全落在解析窗口之下时不再每块整段重解析」。
    /// 放实例上（而非全局静态）：cargo 并行跑测试时全局计数会被其它测试污染。
    #[cfg(test)]
    test_reparse_count: usize,
    /// 测试探针（`cfg(test)`）：置 `true` 强行关闭「纯尾追加门控」得到旧行为基线
    /// （before/after 对比用；生产恒 `false`）。
    #[cfg(test)]
    test_append_gate_disabled: bool,
    /// 测试探针（`cfg(test)`）：回跳「命中快照半程续栈」累计次数（恢复自快照而非整段
    /// 重解析；「回跳命中快照次数 vs 整窗重解析次数」对比用）。
    #[cfg(test)]
    test_snapshot_hit_count: usize,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            syntax: None,
            dark: false,
            base: 0,
            lines: Vec::new(),
            saves: Vec::new(),
            gated_version: 0,
            gated_id: 0,
            snapshots: Vec::new(),
            snapshot_clock: 0,
            dirty: None,
            force_full: true,
            pending: None,
            #[cfg(test)]
            test_reparse_count: 0,
            #[cfg(test)]
            test_append_gate_disabled: false,
            #[cfg(test)]
            test_snapshot_hit_count: 0,
        }
    }

    /// 测试探针：关闭/恢复「纯尾追加门控」（before/after 对比；生产恒关闭生效）。
    #[cfg(test)]
    pub(crate) fn set_test_append_gate_disabled(&mut self, v: bool) {
        self.test_append_gate_disabled = v;
    }

    /// 测试探针：本实例 `full_reparse` 累计次数。
    #[cfg(test)]
    pub(crate) fn test_reparse_count(&self) -> usize {
        self.test_reparse_count
    }

    /// 测试探针：回跳「命中快照半程续栈」累计次数（与 [`Self::test_reparse_count`] 对照：
    /// 回看命中快照 → 本计数 +1 而整段重解析计数不动）。
    #[cfg(test)]
    pub(crate) fn test_snapshot_hit_count(&self) -> usize {
        self.test_snapshot_hit_count
    }

    /// 测试探针：当前缓存行数（自 `base` 起）；用于断言首屏预热**有界**（非全档 parse）。
    #[cfg(test)]
    pub(crate) fn test_cache_len(&self) -> usize {
        self.lines.len()
    }

    /// 测试探针：当前解析状态保存点数（`lines`/`saves` 必须逐行同步，见
    /// `full_reparse` 无语法分支与 `prepare_window` 容量收缩的防御断言）。
    #[cfg(test)]
    pub(crate) fn test_saves_len(&self) -> usize {
        self.saves.len()
    }

    /// 测试探针：逻辑行 `logical` 当前是否已在缓存里且带真颜色段（返回样式段数）。
    /// 分片补色回归用它断言「可见区顶部若干行已有非空颜色」——缓存里有段 = 该行
    /// 本帧被真正解析过（`0` = 未入缓存 / 空样式占位）。
    #[cfg(test)]
    pub(crate) fn test_row_runs(&self, logical: usize) -> usize {
        if self.lines.is_empty() || logical < self.base {
            return 0;
        }
        self.lines
            .get(logical - self.base)
            .map(|runs| runs.len())
            .unwrap_or(0)
    }

    /// 测试探针：累计一次 `full_reparse`（仅测试构建存在）。
    #[cfg(test)]
    fn bump_reparse_count(&mut self) {
        self.test_reparse_count += 1;
    }

    /// 「纯尾追加门控」是否被测试临时关闭（生产恒不关闭）。
    fn append_gate_temporarily_disabled(&self) -> bool {
        #[cfg(test)]
        {
            self.test_append_gate_disabled
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// 语言/色板等切换后丢弃全部增量缓存，下一请求整段重解析（整篇替换文档 / 换语言时
    /// 调用——清空缓存并把 `force_full` 置位，避免「新旧文档同为 version 0」命中旧颜色）。
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.saves.clear();
        self.dirty = None;
        self.pending = None;
        // 整篇替换/换语言 → 历史窗口快照一并作废（换 Buffer 后 version 归零，残留快照
        // 与「同 version 撞车」同危——快照携带 `gated_id`/`dark` 双保险，这里直接清空）。
        self.snapshots.clear();
        self.snapshot_clock = 0;
        self.force_full = true;
    }

    /// 切换语言（按 token/扩展名找语法；`None` 无高亮）。
    pub fn set_language(&mut self, language: Option<&str>) {
        let syntax = language.and_then(find_syntax);
        let same = match (self.syntax, syntax) {
            (Some(a), Some(b)) => std::ptr::eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.syntax = syntax;
            self.invalidate();
        }
    }

    /// 切换明/暗色板（拖主题时调用）。快照存的 `Hsla` 依赖 `dark`，切色板一律作废
    /// （`force_full` 之外再加 `dark` 身份校验双保险）。
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.snapshots.clear();
            self.snapshot_clock = 0;
            self.force_full = true;
        }
    }

    /// 登记「最近一次编辑」的影响范围（`CodeEditor::apply_edit_synced` 在编辑后设置）：
    /// `min_row` = 受影响最小**编辑前**逻辑行；`tail_new` = 编辑后在**当前坐标**下首个
    /// 内容未变的尾部行（`= min_row + 新文本行数`）；`delta` = 净行增量（可负）。
    /// 下一个版本变更后的高亮请求消费一次；`None` 清除（整篇替换/打开/撤销重做走整段
    /// 重解析）。
    pub fn set_dirty(&mut self, dirty: Option<(usize, usize, i64)>) {
        // 编辑（`dirty = Some` 即近期内容变更登记）→ 全部快照失效：历史解析窗口内容已变，
        // 回跳不得复用旧颜色（主窗口仍走既有编辑增量路径）。整篇替换（`None`）由
        // `invalidate` 统一清。
        if dirty.is_some() {
            self.snapshots.clear();
            self.snapshot_clock = 0;
        }
        self.dirty = dirty;
    }

    /// 沉降当前主窗口为快照（LRU）：只在「即将失去当前窗口」（容量前移 drain、整段重锚/
    /// 恢复前）由调用方触发，每次 O(窗口)；滚动前进的命中/`extend_down` 路径零快照开销。
    /// 只对「与当前文档同 id 同版本、未被强刷、行数有界」的有效窗口做快照——编辑后版本
    /// 已跳变 / 换 Buffer / 超大窗口都拒绝（宁慢勿错，保守回落既有整段重解析）。
    fn snapshot_current(&mut self, doc_id: u64, version: u64) {
        if self.lines.is_empty()
            || self.force_full
            || self.gated_id != doc_id
            || self.gated_version != version
            || self.lines.len() > HL_SNAPSHOT_MAX_ROWS
        {
            return;
        }
        self.snapshots.push(HighlightSnapshot {
            doc_id,
            version,
            base: self.base,
            dark: self.dark,
            lines: self.lines.clone(),
            saves: self.saves.clone(),
            last_seen: self.snapshot_clock,
        });
        self.snapshot_clock = self.snapshot_clock.wrapping_add(1);
        self.evict_snapshots_overflow();
    }

    /// LRU 淘汰：快照数超过 `HL_SNAPSHOT_CAP` 时逐出最久未用（cap ≤ 4，线性扫 O(cap)）。
    fn evict_snapshots_overflow(&mut self) {
        while self.snapshots.len() > HL_SNAPSHOT_CAP {
            let mut oldest = 0usize;
            for i in 1..self.snapshots.len() {
                if self.snapshots[i].last_seen < self.snapshots[oldest].last_seen {
                    oldest = i;
                }
            }
            self.snapshots.remove(oldest);
        }
    }

    /// 回跳「半程续栈」：在整段重解析前先试最近解析过的快照——同文档同版本同明暗、且
    /// `base ≤ lo` 的快照里取「对 [lo − lookback, hi] 覆盖最多」者，从其重建主窗口：
    /// 快照覆盖到请求窗底 → 整窗复用（零 syntect）；只覆盖一半 → 从快照最后保存点续解析
    /// （只有窗口超出快照的部分付 syntect，等于 `extend_down` 从快照世界续栈）。
    ///
    /// 正确性约束：
    /// - 快照只做「与主窗口同版本相同的解析」：快照镜像取自真实解析，续栈的保存点起点
    ///   精确（`saves.len() == lines.len()` 不变量在快照里同样成立），`base` 取快照锚点、
    ///   `covered(lo, hi)` 由构造保证（`.base ≤ lo` 提前量 + 行数铺满）→ 返回切片起点不越界；
    /// - 跨快照/主窗口拼接无歧义：只从**单个覆盖最优的快照**续栈，不拆多个快照拼接
    ///   （跨边界有上下文断裂风险）；缓存上界之外统一续栈/整段重解析兜底；
    /// - 任何不确定（无快照命中 / `force_full` / 版本/文档/明暗不符）一律返回 `false` 落到
    ///   既有整段重解析（宁慢勿错）。
    fn try_restore_from_snapshot(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
        version: u64,
    ) -> bool {
        // 即将重锚（整段重解析或恢复）都会丢弃当前主窗口 → 先把它沉降为快照（LRU），
        // 保证「回跳 → 再回跳」的往返各自命中、不打摆子。无效窗口由 `snapshot_current`
        // 内部守卫拒绝。
        self.snapshot_current(buffer.id(), version);
        if self.force_full {
            return false;
        }
        let need_start = lo.saturating_sub(lookback);
        // 有限容量线性扫（cap ≤ 4）：选覆盖最多者；`base > lo` 的快照够不着本请求（起点
        // 在请求窗之下），跳过。
        let mut best: Option<(usize, usize)> = None;
        for (idx, s) in self.snapshots.iter().enumerate() {
            if s.doc_id != buffer.id() || s.version != version || s.dark != self.dark {
                continue;
            }
            if s.base > lo {
                continue;
            }
            let s_end = s.base + s.lines.len();
            if s_end <= need_start {
                continue;
            }
            let cov = s_end.min(hi + 1).saturating_sub(s.base.max(need_start));
            if cov == 0 {
                continue;
            }
            if best.map_or(true, |(_, c)| cov > c) {
                best = Some((idx, cov));
            }
        }
        let Some((best_idx, _)) = best else {
            return false;
        };
        let mut snap = self.snapshots.remove(best_idx);
        #[cfg(test)]
        {
            self.test_snapshot_hit_count += 1;
        }
        let s_end = snap.base + snap.lines.len();
        let mut lines: Vec<Vec<(Range<usize>, Hsla)>> = Vec::with_capacity(hi + 1 - snap.base);
        let mut saves: Vec<ParseState> = Vec::with_capacity(hi + 1 - snap.base);
        // 1) 快照前缀直接复用（零 syntect；base 对齐到快照锚点，`covered` 提前量在其内）。
        let copy_hi = s_end.min(hi + 1);
        for i in 0..(copy_hi - snap.base) {
            lines.push(snap.lines[i].clone());
            saves.push(snap.saves[i].clone());
        }
        // 2) 快照末端 → 请求窗底：从快照最后保存点续解析（`cov > 0` ⇒ 快照非空 → `last()`
        //    恒 Some；同快照世界连续续栈，输出与「回看时所见」+「冷整段重解析」逐位一致）。
        if s_end <= hi {
            let dark = self.dark;
            let mut parser = snap
                .saves
                .last()
                .cloned()
                .unwrap_or_else(|| ParseState::new(SYNTAX_SET.find_syntax_plain_text()));
            for r in s_end..=hi {
                let text = buffer.line_text(r);
                let mut style: Vec<(Range<usize>, Hsla)> = Vec::new();
                self.parse_line(&mut parser, text.as_ref(), &mut style, dark);
                lines.push(style);
                saves.push(parser.clone());
            }
        }
        self.lines = lines;
        self.saves = saves;
        self.base = snap.base;
        self.gated_id = buffer.id();
        self.gated_version = version;
        self.dirty = None;
        self.pending = None;
        self.force_full = false;
        // touch：置为最近使用后放回 LRU（回跳后又回跳仍命中，避免往返各整段重解析一次）。
        snap.last_seen = self.snapshot_clock;
        self.snapshot_clock = self.snapshot_clock.wrapping_add(1);
        self.snapshots.push(snap);
        self.evict_snapshots_overflow();
        debug_assert!(self.covered(lo, hi));
        true
    }

    /// 重锚入口：先试快照半程续栈（回跳/回看命中 → 零或少量 syntect），否则整段重解析
    /// （旧行为）。替换 `full_reparse` 的调用点统一走这里，保证两路径不遗漏。
    fn reparse_or_restore(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
        version: u64,
        line_count: usize,
    ) {
        if self.try_restore_from_snapshot(buffer, lo, hi, lookback, version) {
            return;
        }
        self.full_reparse(buffer, lo, hi, lookback, version, line_count);
    }

    /// 内部缓存起点行（含回看前缀；只用于命中判定，不对返回切片取行）。
    pub fn base(&self) -> usize {
        self.base
    }

    /// 缓存是否已覆盖 `[lo, hi]`（只读判定）。
    fn covered(&self, lo: usize, hi: usize) -> bool {
        lo >= self.base && hi.saturating_add(1) <= self.base.saturating_add(self.lines.len())
    }

    /// 滚动相关的「快速路径」判定：缓存覆盖请求窗口（直接命中），或缺口小到走
    /// `extend_down` 即可（廉价且作用域栈完整）→ 返回 `true`，调用方继续走
    /// `highlight_window`（命中/小扩展两路径）。
    ///
    /// 否则（缺口超单帧预算下跳 / 上移越过 base，需要有界重锚 `full_reparse`）→ 返回
    /// `false`，并把窗口登记为 `pending`（本帧不重锚，帧廉价）；滚动停住后由
    /// [`Self::settle_pending`] 补一次重锚。见 [`Self::pending`] 字段说明。
    pub fn accept_deferred(&mut self, lo: usize, hi: usize, lookback: usize) -> bool {
        if self.covered(lo, hi) {
            self.pending = None;
            return true;
        }
        // 首屏（缓存为空）→ 同步立即上色：打开/首帧一次整窗解析（O(窗口+回看)≈一屏）直接
        // 放本帧做。P1c 曾把「首帧一律延后 + settle」与「滚动大缺口延后」共用同一
        // pending/settle 状态机，导致首次打开大文档也是先出纯文本、滚动停住才上色
        // （观感死区）；改后只有**滚动大缺口**（>单帧预算下跳 / 上移越过 base）才走
        // 延后 + settle（滚动期帧必须廉价、滑块/内容跟手）。
        // `.txt`（无 syntax）首屏本就走快路径（建缓存只是填空样式，代价≈0），不受影响。
        if self.lines.is_empty() {
            return true;
        }
        let needs_reanchor = if lo >= self.base {
            let cached_end = self.base + self.lines.len();
            hi.saturating_add(1) > cached_end
                && hi.saturating_sub(cached_end).saturating_add(1) > EXTEND_SYNC_BUDGET_ROWS
        } else {
            // 上移出缓存：`highlight_window` 也会走整段重解析，同样贵 → 同样延后。
            true
        };
        if needs_reanchor {
            self.pending = Some((lo, hi, lookback));
            false
        } else {
            true
        }
    }

    /// 消费并执行一次被延后的有界重锚（`full_reparse`），返回是否已补齐缓存。
    /// 滚动安定（相邻帧请求窗口一致）时由调用方触发：本帧用真颜色、之后命中缓存。
    pub fn settle_pending(&mut self, buffer: &Buffer) -> bool {
        let Some((lo, hi, lookback)) = self.pending.take() else {
            return false;
        };
        let version = buffer.version();
        let line_count = buffer.line_count();
        // 回跳 settle（上移出缓存）优先试快照半程续栈，命中则不用整段重解析。
        self.reparse_or_restore(buffer, lo, hi, lookback, version, line_count);
        self.covered(lo, hi)
    }

    /// 当前被延后的高亮窗口（滚动中为 `Some`；供调用方判断「相邻帧窗口一致→滚动已停」）。
    pub fn pending(&self) -> Option<(usize, usize, usize)> {
        self.pending
    }

    /// 请求可见行窗口 `[lo, hi]`（含两端）的高亮。返回**与请求窗口对齐**的
    /// 切片：第 0 项 = `lo` 行、长度 = `hi - lo + 1`（缓存命中返回子切片，未命中
    /// 从 `lookback` 回看起点重解析后也裁剪成同样对齐——两条路径必须一致）。
    ///
    /// 内容未变（滚动）→ 命中 / 向下扩展；版本变且有 `set_dirty` 登记 → 编辑增量
    /// （局部重解析 + 收敛早退）；否则整段重解析。
    pub fn highlight_window(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
    ) -> &[Vec<(Range<usize>, Hsla)>] {
        let line_count = buffer.line_count();
        if line_count == 0 {
            self.lines.clear();
            self.saves.clear();
            self.base = 0;
            self.gated_version = 0;
            self.dirty = None;
            return &self.lines;
        }
        let lo = lo.min(line_count - 1);
        let hi = hi.min(line_count - 1);
        let version = buffer.version();
        // WY_TRACE breakdown：语法高亮窗口解析段（命中/扩展/编辑增量/整段重解析）。
        let _t_hl = crate::prof::begin();

        // 全部可变操作集中在 prepare（命中 / 扩展 / 编辑增量 / 整段重解析），
        // 结束后才借 `self.lines` 返回切片——避免「返回切片潜伏借用」与可变操作冲突。
        self.prepare_window(buffer, lo, hi, lookback, version, line_count);

        // 读取对齐切片（prepare 保证缓存覆盖 [lo, hi] 且 base ≤ lo）。
        let rel = lo - self.base;
        let need = hi - lo + 1;
        crate::prof::end("ed.hl_window", _t_hl);
        &self.lines[rel..rel + need]
    }

    /// 确保缓存覆盖 `[lo, hi]`（做一些可变重建）。见 [`Self::highlight_window`] 的
    /// 三条路径。
    fn prepare_window(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
        version: u64,
        line_count: usize,
    ) {
        // 容量自愈：缓存保存点远超「请求窗口 + 回看」时，把 base 前移到 `lo - lookback`
        // 并丢弃顶部旧行（缓存的最高行不变，覆盖 [lo, hi] 不受影响）——防止大文档一路
        // 向下滚动让 `extend_down` 把保存点缓存单调涨到 O(全文)（内存 + 事后增量前缀
        // 深拷贝成本）。滚动回跳/编辑在 `lo - lookback` 之上时回落到整段重解析即可。
        if !self.force_full && !self.lines.is_empty() {
            let needed = (hi - lo + 1).saturating_add(lookback);
            let cap = needed.saturating_mul(2).max(needed.saturating_add(64));
            if self.lines.len() > cap {
                // 防御：`lines`/`saves` 必须逐行同步（`full_reparse` 无语法分支曾只填
                // lines 不填 saves，导致 `saves.drain(..drop)` 越界 panic）。debug 下
                // 断言不变量；release 下 `drop` 同时按 `saves.len()` 钳位防越界再现。
                debug_assert_eq!(self.lines.len(), self.saves.len());
                let new_base = lo.saturating_sub(lookback).max(self.base);
                let drop = new_base
                    .saturating_sub(self.base)
                    .min(self.lines.len())
                    .min(self.saves.len());
                if drop > 0 {
                    // 即将丢弃顶部旧行（离开保护区）→ 先把整窗沉降为快照，回跳回这片区域
                    // 从快照半程续栈，不再整段重解析。无语法空样式窗口沉降快照无益但无害
                    // （`snapshot_current` 按行数/版本/文档/明暗自检）。
                    self.snapshot_current(buffer.id(), version);
                    self.lines.drain(..drop);
                    self.saves.drain(..drop);
                    self.base = new_base;
                }
            }
        }

        // A0) 渐进打开「纯尾追加」门控（P1）：版本虽推进，但变更完全落在当前已解析窗口
        // `[base, base+lines.len())` 之下——`append_chunk` 只在全文尾部追加新行、窗口内
        // 既有行内容未动 → 缓存仍准确，无需对可视窗口整段重解析（大 .rs 每块 ~91ms）。
        // 把 `gated_version` 对齐到当前版本，接着走「内容未变」路径（命中 / `extend_down`
        // 续解析，滚进新块区域时颜色正确落位）。条件：
        // - 无在途编辑登记（编辑即使被 `apply_edit` 清零标记，本帧 `set_dirty` 也已喂入，
        //   双保险）且未强刷；
        // - 追加运行自上次缓存版本之后开始（`start_version ≤ gated_version+1`；若不成立，
        //   说明中间隔着一次非追加变更，缓存可能过期）；
        // - 追加起点行 ≥ 当前缓存上界（`base + lines.len()`，追加区**完全**落在已解析窗口
        //   之下）。非恒成立：缓存若在追加运行**期间**重建过（缓存尾已含部分追加行），
        //   此护栏拒绝命中、保守落整段重解析（代价=旧行为一次重解析，保住正确性）。
        if !self.force_full
            && !self.lines.is_empty()
            && version != self.gated_version
            && self.dirty.is_none()
            && !self.append_gate_temporarily_disabled()
            && let Some((start_version, start_row)) = buffer.tail_append()
            && start_version <= self.gated_version.wrapping_add(1)
            && start_row >= self.base + self.lines.len()
        {
            self.gated_version = version;
        }

        // A) 内容未变：命中缓存；窗口下移出缓存则向下扩展（只解析新行）。
        if !self.force_full && version == self.gated_version && !self.lines.is_empty() {
            if self.covered(lo, hi) {
                return;
            }
            // 向下扩展：新请求窗口底超出缓存上界（滚动下移路径）。
            if lo >= self.base {
                let cached_end = self.base + self.lines.len();
                if hi + 1 > cached_end {
                    // 缺口超过单帧同步预算（拖滚动条/中速大跳）→ 有界重锚（O(窗口+回看)），
                    // 避免逐行解析整个缺口卡死 UI 线程；缺口 ≤ 预算则正常向下扩展
                    //（滚轮/惯性微跳，作用域栈完整）。
                    let gap = hi.saturating_sub(cached_end).saturating_add(1);
                    if gap > EXTEND_SYNC_BUDGET_ROWS {
                        // 有界重锚（O(窗口+回看)）：先试快照半程续栈（命中回跳区域则零/少
                        // syntect），否则整段重解析——避免逐行解析整个缺口卡死 UI 线程。
                        self.reparse_or_restore(buffer, lo, hi, lookback, version, line_count);
                        return;
                    }
                    self.extend_down(buffer, cached_end, hi);
                    if self.covered(lo, hi) {
                        return;
                    }
                }
            }
            // 还差 → 窗口上移越过 base，落到整段重解析（D）。
        }

        // B) 编辑增量：恰逢「自上次缓存以来**恰好一次**编辑」且登记了最近编辑 →
        // 局部重解析 + 收敛早退。多次编辑在同一帧合并（长按/IME 组合/编辑+滚动同帧）
        // 会使单槽 `hl_dirty` 只反映最后一次编辑的 delta/位移 → 位移映射失真，可能
        // 造成错误收敛（错位复制）；此时回落到整段重解析（正确且代价=旧实现）。
        if !self.force_full
            && version == self.gated_version.wrapping_add(1)
            && !self.lines.is_empty()
            && lo >= self.base
            && let Some((min_row, tail_new, delta)) = self.dirty
            && (min_row > self.base
                || (min_row == self.base && self.base == 0)) // P4-3：文首编辑无上游作用域，增量安全
            && min_row.saturating_sub(lookback) < self.base + self.lines.len()
        {
            // 编辑增量：从 `min_row`（编辑行）直接用**编辑前一行保存点**
            // `old_saves[min_row-1]` 恢复作用域（min_row-1 及之前内容未变，快照即准确），
            // 不再从 `min_row - lookback` 重解析回看行——普通字符输入一行的重解析即可
            // 收敛（lookback 只在窗口顶 base 边界才需要，此时 min_row == base 用全新状态）。
            // 原实现每键重解析 16 行回看 + 窗口行，debug 下 syntect 逐行 parse 是
            // 大文档 .rs 输入卡顿主因（hl_reparse_ok ~36ms/次）。
            // 注意：条件用**严格 `min_row > base`**——若编辑恰在缓存起点行（min_row==base），
            // 上一行保存点 `min_row-1` 不存在，用全新 ParseState 会丢多行注释/字符串
            // 作用域，此时落 full_reparse（从 lo-lookback 重建，上下文完整，与原实现一致）。
            let reenter = min_row;
            let ok = self.reparse_after_edit(buffer, reenter, hi, min_row, tail_new, delta);
            if ok && self.covered(lo, hi) {
                self.gated_version = version;
                self.dirty = None;
                return;
            }
            // 未达成（语言未配置 / 切片仍不够）→ 落到整段重解析（D）；重解析会清空 dirty。
        }

        // D) 整段重解析（或回跳快照半程续栈）：从 lo - lookback 起建缓存（打开/整篇
        // 替换/滚动上移出缓存/编辑在缓存起点之上等）。代价 O(窗口 + 回看)，与文档总大小
        // 无关；回跳命中快照时降到零/少量 syntect。
        self.reparse_or_restore(buffer, lo, hi, lookback, version, line_count);
    }

    /// 整段重解析建缓存：锚点 = `lo - lookback`，覆盖到请求窗口底 `hi`。
    /// 代价 O(窗口 + 回看)，与文档/缺口总大小无关；供打开/整篇替换/滚动上下移出缓存/
    /// 大缺口下跳回锚共用。**注意**：从文档深处用全新 `ParseState` 起锚，多行构造
    /// （块注释/多行字符串）的着色在作用域栈落定前是近似值——与「向上滚出缓存→整段
    /// 重解析」既有的取舍一致。
    fn full_reparse(
        &mut self,
        buffer: &Buffer,
        lo: usize,
        hi: usize,
        lookback: usize,
        version: u64,
        line_count: usize,
    ) {
        #[cfg(test)]
        self.bump_reparse_count();
        let start = lo.saturating_sub(lookback).min(line_count.saturating_sub(1));
        // 方案 B：首屏从文件头锚定（空缓存 + lo==0）时预热 ≤2×视口 行（视口 =
        // hi−lo+1，封顶 `EXTEND_SYNC_BUDGET_ROWS`）并保存每行作用域保存点——打开后首次
        // 下滚在预热范围内由 `extend_down`
        // 从保存点连续续栈、不整段重锚（滚动小缺口即时上色而不是重锚整窗）。预热必须
        // 封顶（`min` 单帧预算），且**绝不越过「末真实行」**：`line_count` 含 ropey
        // 尾换行幻影空行，若把幻影行缓存下来，追加开始时该槽被新内容占用而缓存未失效
        // → 复用旧空槽（色缺失）。预热下端最多到请求窗口底（请求窗本身含幻影行是既有
        // 语义，不得缩到请求窗口之下）。代价 O(视口)+常数，绝不演变成全档 parse。
        let requested_hi = hi;
        let warm_rows = if self.lines.is_empty() && lo == 0 {
            (hi.saturating_sub(lo).saturating_add(1).saturating_mul(2))
                .min(EXTEND_SYNC_BUDGET_ROWS)
        } else {
            0
        };
        // 末真实行：`line_count - 1` 为幻影空行（尾换行 → 该行长 0）则减一，否则即末行。
        let last_real = if line_count > 1 && buffer.line_len(line_count - 1) == 0 {
            line_count.saturating_sub(2)
        } else {
            line_count.saturating_sub(1)
        };
        let warm_ceiling = last_real.max(requested_hi);
        let hi = requested_hi.saturating_add(warm_rows).min(warm_ceiling);
        let mut lines: Vec<Vec<(Range<usize>, Hsla)>> = Vec::with_capacity(hi - start + 1);
        let mut saves: Vec<ParseState> = Vec::with_capacity(hi - start + 1);
        let Some(syntax) = self.syntax else {
            // 无语法（.txt 等）：每行填空样式即可，但必须同步 push 一个「纯文本解析器」
            // 占位保存点，恢复 `saves.len() == lines.len()` 不变量——否则深跳触发容量
            // 收缩 `saves.drain(..drop)` 会越界 panic（syntax 为 None 时 `extend_down`
            // /`reparse_after_edit` 都早退，占位保存点只用于对齐、从不被读取）。
            let plain = SYNTAX_SET.find_syntax_plain_text();
            let fresh = ParseState::new(plain);
            for _ in start..=hi {
                lines.push(Vec::new());
                saves.push(fresh.clone());
            }
            self.lines = lines;
            self.saves = saves;
            self.base = start;
            self.gated_version = version;
            self.gated_id = buffer.id();
            self.dirty = None;
            self.force_full = false;
            return;
        };

        let dark = self.dark;
        let mut parser = ParseState::new(syntax);
        for row in start..=hi {
            let text = buffer.line_text(row);
            let mut runs: Vec<(Range<usize>, Hsla)> = Vec::new();
            self.parse_line(&mut parser, text.as_ref(), &mut runs, dark);
            lines.push(runs);
            saves.push(parser.clone());
        }
        self.lines = lines;
        self.saves = saves;
        self.base = start;
        self.gated_version = version;
        self.gated_id = buffer.id();
        self.dirty = None;
        self.pending = None;
        self.force_full = false;
    }

    /// 向下扩展缓存到 `hi`（内容未变、滚动下移路径）：从最后一个保存点继续解析，
    /// 只新增行。`from` = 下一个待解析逻辑行（`== base + lines.len()`）。
    fn extend_down(&mut self, buffer: &Buffer, from: usize, hi: usize) {
        let Some(syntax) = self.syntax else {
            return;
        };
        let dark = self.dark;
        let mut parser = self
            .saves
            .last()
            .cloned()
            .unwrap_or_else(|| ParseState::new(syntax));
        for row in from..=hi {
            let text = buffer.line_text(row);
            let mut runs: Vec<(Range<usize>, Hsla)> = Vec::new();
            self.parse_line(&mut parser, text.as_ref(), &mut runs, dark);
            self.lines.push(runs);
            self.saves.push(parser.clone());
        }
    }

    /// 编辑后的局部重解析：从 `reenter` 恢复编辑前保存点，重解析到 `hi`，并在
    /// 「已越过编辑新文本跨（`r+1 ≥ tail_new`）且本行样式 + 解析状态与旧缓存位移
    /// 对应行一致」处收敛（提前终止，尾部行从旧缓存按位移复制）。
    ///
    /// - `min_row`：编辑前最小受影响行；`tail_new`：当前坐标下首尾行；`delta`：净行增量。
    /// - 新行 `r` → 旧缓存行 `shift(r)`：`r < min_row` 恒等（未变区），否则 `r − delta`
    ///   （尾部，内容同位移旧行）。
    /// - 收敛是**充分**的：编辑跨已全重解析（`r+1 ≥ tail_new`）⇒ 其下全是内容未变的
    ///   尾部行；本行与旧缓存一致 ⇒ 同解析器 + 相同后续行 → 输出逐行确定，故下方不变。
    /// - 不收敛则一直重解析到 `hi`（最坏 = 整窗，等价旧实现）。
    fn reparse_after_edit(
        &mut self,
        buffer: &Buffer,
        reenter: usize,
        hi: usize,
        min_row: usize,
        tail_new: usize,
        delta: i64,
    ) -> bool {
        let Some(syntax) = self.syntax else {
            return false;
        };
        let dark = self.dark;
        // 整体取出旧缓存（重建期间不能借用 self）。
        let old_lines = std::mem::take(&mut self.lines);
        let old_saves = std::mem::take(&mut self.saves);
        let old_base = self.base;
        let old_len = old_lines.len();

        // 恢复 `reenter` 前一行的解析状态（reenter == base → 全新状态）。
        let mut parser = if reenter == old_base {
            ParseState::new(syntax)
        } else {
            match old_saves.get(reenter - 1 - old_base) {
                Some(s) => s.clone(),
                None => return false, // 不应发生（调用方已保证 reenter ≥ base）
            }
        };

        // 编辑线之前（未受影响）部分直接沿用旧缓存。
        let prefix = (reenter - old_base).min(old_len);
        let mut lines: Vec<Vec<(Range<usize>, Hsla)>> = old_lines[..prefix].to_vec();
        let mut saves: Vec<ParseState> = old_saves[..prefix].to_vec();

        // 新行 r → 旧缓存行（恒等 / 位移）。
        let shift = |r: usize| -> i64 {
            if r < min_row {
                r as i64
            } else {
                r as i64 - delta
            }
        };

        let mut r = reenter;
        let mut converged_at = None;
        while r <= hi {
            let text = buffer.line_text(r);
            let mut style: Vec<(Range<usize>, Hsla)> = Vec::new();
            self.parse_line(&mut parser, text.as_ref(), &mut style, dark);
            let save = parser.clone();
            let idx = r - old_base;
            if idx == lines.len() {
                lines.push(style);
                saves.push(save);
            } else {
                lines[idx] = style;
                saves[idx] = save;
            }
            // 收敛判定：编辑新文本跨已全重解析，且本行与旧缓存位移对应行一致。
            if r + 1 >= tail_new {
                let or = shift(r);
                if or >= old_base as i64 && (or as usize) - old_base < old_len {
                    let oi = (or as usize) - old_base;
                    if lines[idx] == old_lines[oi] && saves[idx] == old_saves[oi] {
                        converged_at = Some(r);
                        break;
                    }
                }
            }
            r += 1;
        }

        // 收敛 → 尾部（r+1..=hi）从旧缓存按位移复制（内容与解析状态都没变）。
        // 每行都做界校验：若某尾部行映射到旧缓存之外（例如编辑与滚动在同一帧
        // 合并、请求窗口底超出旧缓存上界），说明尾部不再能安全复用 → 放弃增量，
        // 返回 false 让上层整段重解析（杜绝越界 panic）。
        if let Some(cr) = converged_at {
            for rr in (cr + 1)..=hi {
                let or = shift(rr);
                if or < old_base as i64 || (or as usize) - old_base >= old_len {
                    return false;
                }
                let oi = (or as usize) - old_base;
                lines.push(old_lines[oi].clone());
                saves.push(old_saves[oi].clone());
            }
        }

        self.lines = lines;
        self.saves = saves;
        // base 保持 old_base（前缀沿用，切片仍对齐）。
        true
    }

    fn parse_line(
        &self,
        parser: &mut ParseState,
        text: &str,
        out: &mut Vec<(Range<usize>, Hsla)>,
        dark: bool,
    ) {
        let Ok(operations) = parser.parse_line(text, &SYNTAX_SET) else {
            return;
        };
        let mut active: Vec<Scope> = Vec::new();
        let mut cursor = 0usize;
        let line_len = text.len();
        let paint = |active: &[Scope], from: usize, to: usize, out: &mut Vec<(Range<usize>, Hsla)>| {
            if to <= from {
                return;
            }
            let name = active.iter().rev().find_map(semantic_name);
            if let Some(name) = name
                && let Some(color) = name_color(name, dark) {
                    out.push((from..to, color));
                }
        };
        for (index, operation) in operations {
            if cursor < index {
                paint(&active, cursor, index, out);
            }
            self.apply_op(&mut active, &operation);
            cursor = index;
        }
        if cursor < line_len {
            paint(&active, cursor, line_len, out);
        }
    }

    fn apply_op(&self, active: &mut Vec<Scope>, op: &ScopeStackOp) {
        match op {
            ScopeStackOp::Push(scope) => active.push(*scope),
            ScopeStackOp::Pop(count) => {
                if *count >= active.len() {
                    active.clear();
                } else {
                    active.truncate(active.len() - *count);
                }
            }
            ScopeStackOp::Clear(_) | ScopeStackOp::Restore => active.clear(),
            ScopeStackOp::Noop => {}
        }
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}
