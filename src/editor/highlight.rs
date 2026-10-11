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
//! 增量的情况回落到整段重解析（与旧实现一致，只影响该路径的代价、不影响正确性）。
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

/// 向下滚动「缺口」超过此值（目标窗口底 − 缓存上界 + 1）时，放弃逐行 `extend_down`
/// 改走**有界重锚**（`full_reparse`，O(窗口+回看)）：大文档 `.rs` 拖滚动条时滑块极短，
/// 1 鼠标像素 ≈ 数千行，若逐行把整个缺口重跑 syntect 会把 UI 线程卡死数百毫秒
/// （表现成「内容只有松开才动」）。缺口小（滚轮/小幅拖拽）仍走 extend_down，作用域栈
/// 完整、颜色正确。
const REANCHOR_GAP: usize = 1_024;

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
    /// 待消费的「最近编辑」登记（由 `set_dirty` 写入，仅消费一次）：
    /// `(min_row, tail_new, delta)` —— `min_row` 编辑前最小受影响逻辑行；
    /// `tail_new` 编辑后在**当前坐标**下首个内容未变的「尾部」行
    /// （= `min_row + 新文本行数`）；`delta` 净行增量（新行数 − 被删行数）。
    dirty: Option<(usize, usize, i64)>,
    /// 语言/色板切换强制整窗重解析（跳过增量路径）。
    force_full: bool,
    /// 滚动（拖拽/快滚）期间被延后的高亮窗口 `(lo, hi, lookback)`：大文档上拖滚动条
    /// 每提交一跳都远超缓存，若每帧同步 `full_reparse`（DEBUG 下 syntect ~百 ms/次）
    /// 会把帧率压到个位数，滑块/内容跟不紧鼠标。先登记 pending、本帧用空样式渲染
    /// （帧廉价），滚动停住（相邻帧窗口一致）再由 `settle_pending` 补一次有界重锚。
    pending: Option<(usize, usize, usize)>,
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
            dirty: None,
            force_full: true,
            pending: None,
        }
    }

    /// 语言/色板等切换后丢弃全部增量缓存，下一请求整段重解析（整篇替换文档 / 换语言时
    /// 调用——清空缓存并把 `force_full` 置位，避免「新旧文档同为 version 0」命中旧颜色）。
    pub fn invalidate(&mut self) {
        self.lines.clear();
        self.saves.clear();
        self.dirty = None;
        self.pending = None;
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

    /// 切换明/暗色板（拖主题时调用）。
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.force_full = true;
        }
    }

    /// 登记「最近一次编辑」的影响范围（`CodeEditor::apply_edit_synced` 在编辑后设置）：
    /// `min_row` = 受影响最小**编辑前**逻辑行；`tail_new` = 编辑后在**当前坐标**下首个
    /// 内容未变的尾部行（`= min_row + 新文本行数`）；`delta` = 净行增量（可负）。
    /// 下一个版本变更后的高亮请求消费一次；`None` 清除（整篇替换/打开/撤销重做走整段
    /// 重解析）。
    pub fn set_dirty(&mut self, dirty: Option<(usize, usize, i64)>) {
        self.dirty = dirty;
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
    /// 否则（大缺口下跳 / 上移越过 base，需要有界重锚 `full_reparse`）→ 返回 `false`，
    /// 并把窗口登记为 `pending`（本帧不重锚，帧廉价）；滚动停住后由
    /// [`Self::settle_pending`] 补一次重锚。见 [`Self::pending`] 字段说明。
    pub fn accept_deferred(&mut self, lo: usize, hi: usize, lookback: usize) -> bool {
        if self.covered(lo, hi) {
            self.pending = None;
            return true;
        }
        let needs_reanchor = if lo >= self.base {
            let cached_end = self.base + self.lines.len();
            hi.saturating_add(1) > cached_end
                && hi.saturating_sub(cached_end).saturating_add(1) > REANCHOR_GAP
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
        self.full_reparse(buffer, lo, hi, lookback, version, line_count);
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
                let new_base = lo.saturating_sub(lookback).max(self.base);
                let drop = new_base.saturating_sub(self.base).min(self.lines.len());
                if drop > 0 {
                    self.lines.drain(..drop);
                    self.saves.drain(..drop);
                    self.base = new_base;
                }
            }
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
                    // 缺口巨大（拖滚动条在大文档上暴跳）→ 有界重锚，避免逐行解析整个
                    // 前缀卡死 UI 线程；缺口小则正常向下扩展（作用域栈完整）。
                    let gap = hi.saturating_sub(cached_end).saturating_add(1);
                    if gap > REANCHOR_GAP {
                        self.full_reparse(buffer, lo, hi, lookback, version, line_count);
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

        // D) 整段重解析：从 lo - lookback 起建缓存（打开/整篇替换/滚动上移出缓存/
        // 编辑在缓存起点之上等）。代价 O(窗口 + 回看)，与文档总大小无关。
        self.full_reparse(buffer, lo, hi, lookback, version, line_count);
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
        let start = lo.saturating_sub(lookback).min(line_count.saturating_sub(1));
        let mut lines: Vec<Vec<(Range<usize>, Hsla)>> = Vec::with_capacity(hi - start + 1);
        let mut saves: Vec<ParseState> = Vec::with_capacity(hi - start + 1);
        let Some(syntax) = self.syntax else {
            for _ in start..=hi {
                lines.push(Vec::new());
            }
            self.lines = lines;
            self.saves = saves;
            self.base = start;
            self.gated_version = version;
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
