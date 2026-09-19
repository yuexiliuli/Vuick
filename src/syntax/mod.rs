//! 编辑器代码高亮：syntect 语法解析 + GitHub 风格配色，**窗口式高亮**。
//!
//! gpui-component 的 CodeEditor 是**解析无关**的：只要提供实现
//! [`gpui_component::input::InputHighlighter`] 的解析器（这里用 syntect 的默认语法集）
//! 与一个 [`gpui_component::highlighter::HighlightTheme`] 配色（GitHub Light/Dark），
//! 编辑区即可着色。组件自带的 tree-sitter 工厂在 `tree-sitter` 特性关闭时是空操作，
//! 因此由本模块负责提供真实的语法解析与配色。
//!
//! ## 性能设计（窗口式同步高亮，任意文档大小都有高亮且不卡 UI）
//!
//! **不整篇解析**：只在渲染时同步解析**当前可见窗口**（可见行 + 上下过扫 + 窗口前一
//! 段 [`LOOKBACK_LINES`] 行作状态前缀）。一次最多 ~116 行 ≈ 十几 ms、命中缓存后后续帧
//! 直接复用，与文档总大小无关——6MB / 16 万行文档同样即时配色。
//! 因而**不引入后台线程/轮询**：快速滚动时每个新视口同步重算，无跨线程往返延迟，
//! 也就没有“着色滞后”的幽灵延迟（代价是滚动时帧率略降，换取颜色始终跟手）。
//!
//! - [`highlighter_factory`]：创建编辑器（`app::tabs::make_tab`）时安装
//! - [`install_github_theme`]：启动（`main.rs`）与切换明/暗主题（`app::toggle_theme`）时安装

#![allow(dead_code)] // gpui-component 编辑高亮引擎已被自研 editor 替代，保留未接线部分（markdown 预览仍用其配色/块解析）

mod markdown;

use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::{Arc, LazyLock},
};

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_component::highlighter::{
    FontStyle, HighlightTheme, HighlightThemeStyle, StatusColors, SyntaxColors, ThemeStyle,
};
use gpui_component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::config::limits::{LOOKBACK_LINES, MAX_TEXT_BYTES, SYNC_MAX_PARSE_LINES, WINDOW_PAD};

/// 编辑器编辑侧通道：最近一次 `InputEdit` 的字节区间 `(start, old_end, new_end)`。
///
/// gpui-component 的文本替换先于 `InputEvent::Change` 调高亮器 `update(Some(edit))`
/// （`replace_text_in_range` 内 `drive_highlighter` → `emit(Change)`），因此
/// 「读一次并清空」与 Change 事件严格 1:1；`set_value`/undo/redo 走 silent 路径
/// 不发射 Change，不会污染。去抖窗口内多次编辑由预览同步（`app::tabs`）按字节
/// 坐标累积成 **dirty 区间**，喂给 [`crate::markdown::scan_blocks_incremental_rope`]
/// 跳过 O(n) 快照 diff。
pub(crate) type EditTracker = Rc<std::cell::RefCell<Option<(usize, usize, usize)>>>;

/// 共享语法集（默认语法定义有几 MB，跨实例共享一次）。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// 语言名 → 语法的解析结果缓存（`find_syntax_by_token` 全量扫描较慢，避免每次重扫）。
static SYNTAX_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Option<&'static SyntaxReference>>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 应用侧 `language_id`（见 [`crate::app::EditorApp::language_id`]）与
/// syntect 语法名的少量别名归一。
fn normalize_language(lang: &str) -> &str {
    match lang {
        "sh" | "bash" => "Shell Script",
        "yml" => "yaml",
        "js" => "javascript",
        "ts" => "typescript",
        _ => lang,
    }
}

/// 按语言 token 或扩展名定位语法（带结果缓存）；找不到返回 `None`（不产出高亮）。
fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let lang = normalize_language(language).to_owned();
    if let Ok(cache) = SYNTAX_CACHE.lock() {
        if let Some(r) = cache.get(&lang) {
            return *r;
        }
    }
    let r = SYNTAX_SET
        .find_syntax_by_token(&lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(&lang));
    if let Ok(mut cache) = SYNTAX_CACHE.lock() {
        cache.insert(lang, r);
    }
    r
}

/// 可见窗口解析缓存（后台任务“算好→写回”，UI 线程读取；仅同线程访问）。
struct WindowCache {
    /// 当前文本快照（ropey COW 共享克隆 O(1)，**不整份复制**；编辑时编辑器
    /// 的 Rope 与这里共享底层块，省掉一份全文副本，6MB 文档即省 6MB）。
    /// 用于判断文本是否变化（ropey 实现了按内容的 `PartialEq`）。
    rope: Rope,
    /// 行 i 的行首字节偏移；`line_starts.len()` = 行数（末行结束 = `rope.len()`）。
    line_starts: Vec<usize>,
    /// 折叠使能（当前文本）。
    folding: bool,
    /// 已产出对应的窗口行区间（含过扫余量）；当前请求在内则无需重算。
    applied: Option<(usize, usize)>,
    /// 窗口行内的高亮段（文档字节坐标 → 语义名）。
    win_styles: Vec<(Range<usize>, &'static str)>,
    /// 窗口内的折叠段（行号基于文档）。
    win_folds: Vec<FoldRange>,
}

impl WindowCache {
    fn new() -> Self {
        Self {
            rope: Rope::from(""),
            line_starts: Vec::new(),
            folding: false,
            applied: None,
            win_styles: Vec::new(),
            win_folds: Vec::new(),
        }
    }

    /// 文本变化时重建行索引并清空产出，让下一次渲染按新文本重算顶部窗口。
    fn reset_for(&mut self, rope: Rope, folding: bool) {
        self.line_starts = build_line_starts_rope(&rope);
        self.rope = rope;
        self.folding = folding;
        self.applied = None;
        self.win_styles.clear();
        self.win_folds.clear();
    }
}

/// 活跃的 [`InputHighlighter`]：**同步**解析可见窗口并缓存（无后台线程/轮询）。
/// 渲染时 [`Self::styles`] 若发现缓存窗口不覆盖当前可见区，就在当前帧内同步重算，
/// 因此快速滚动时颜色始终跟手、无跨线程滞后。
struct SyntectHighlighter {
    language: SharedString,
    cache: Rc<RefCell<WindowCache>>,
    tracker: EditTracker,
}

impl SyntectHighlighter {
    fn new(language: &str, tracker: EditTracker) -> Option<Self> {
        find_syntax(language)?;
        Some(Self {
            language: language.to_owned().into(),
            cache: Rc::new(RefCell::new(WindowCache::new())),
            tracker,
        })
    }

    /// 同步解析可见窗口（含过扫与 lookback 前缀）并写回缓存。代价有界、与文档总大小
    /// 无关；只物化「可见+前缀」的连续窗口文本（不整份复制）。
    /// 超过 [`SYNC_MAX_PARSE_LINES`] 的大视口直接跳过（本次沿用旧缓存）。
    fn compute_and_store(&self, req_lo: usize, req_hi: usize) {
        let update = {
            let c = self.cache.borrow();
            compute_window_doc(
                &c.rope,
                &c.line_starts,
                req_lo,
                req_hi,
                self.language.as_ref(),
                c.folding,
            )
        };
        let Some(((styles, folds), plo, phi)) = update else {
            return;
        };
        let mut c = self.cache.borrow_mut();
        c.win_styles = styles;
        c.win_folds = folds;
        c.applied = Some((plo, phi));
    }
}

/// 在 rope 上解析「可见窗口 + 过扫 + lookback 前缀」区域，产出 **文档坐标** 的
/// 高亮段与折叠段。纯函数，独立于缓存；返回 None 表示无需更新（文档为空 /
/// 超过单次解析上限）。`plo/phi` 为本次覆盖的行区间（含过扫）。
///
/// 只物化 ≤ ~116 行的连续窗口文本（ropey `slice` 用字节偏移且行首是 char 边界），
/// 不会整份复制长文档——省掉编辑器高亮缓存里的一份全文副本。
fn compute_window_doc(
    rope: &Rope,
    line_starts: &[usize],
    req_lo: usize,
    req_hi: usize,
    language: &str,
    folding: bool,
) -> Option<((Vec<(Range<usize>, &'static str)>, Vec<FoldRange>), usize, usize)> {
    if rope.len() == 0 || line_starts.is_empty() {
        return None;
    }
    let line_count = line_starts.len();
    let plo = req_lo.saturating_sub(WINDOW_PAD);
    let phi = (req_hi + WINDOW_PAD).min(line_count - 1);
    let parse_lo = plo.saturating_sub(LOOKBACK_LINES);
    if phi.saturating_sub(parse_lo) + 1 > SYNC_MAX_PARSE_LINES {
        return None;
    }
    let start_byte = line_starts[parse_lo.min(line_count - 1)];
    let end_byte = if phi + 1 < line_count {
        line_starts[phi + 1]
    } else {
        rope.len()
    };
    let window = rope.slice(start_byte..end_byte).to_string();
    let win_starts = build_line_starts(&window);
    let (styles, folds) = compute_window(
        &window,
        &win_starts,
        plo.saturating_sub(parse_lo),
        phi.saturating_sub(parse_lo),
        language,
        folding,
    );
    // 窗口相对坐标 → 文档坐标
    let styles = styles
        .into_iter()
        .map(|(r, n)| ((r.start + start_byte)..(r.end + start_byte), n))
        .collect();
    let folds = folds
        .into_iter()
        .map(|f| FoldRange::new(f.start_line + parse_lo, f.end_line + parse_lo))
        .collect();
    Some(((styles, folds), plo, phi))
}

/// 请求是否已被满足：已产出的窗口（含扩展余量）包含当前可见窗口。
fn requirement_met(
    line_count: usize,
    applied: Option<(usize, usize)>,
    req_lo: usize,
    req_hi: usize,
) -> bool {
    if line_count == 0 {
        return true;
    }
    match applied {
        Some((alo, ahi)) => alo <= req_lo && ahi >= req_hi,
        None => false,
    }
}

impl InputHighlighter for SyntectHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    /// 每次编辑/加载都会调用：只重建行索引并登记文本，**不做解析**
    /// （解析推迟到渲染时按可见窗口同步做）。
    fn update(
        &mut self,
        edit: Option<InputEdit>,
        text: &Rope,
        folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // 编辑侧通道：把本次编辑的字节区间交给预览同步（先于 InputEvent::Change）。
        if let Some(e) = &edit {
            *self.tracker.borrow_mut() = Some((e.start_byte, e.old_end_byte, e.new_end_byte));
        }
        if text.len() > MAX_TEXT_BYTES {
            // 超大纲：清掉高亮（保持即时响应）
            let mut c = self.cache.borrow_mut();
            if c.rope.len() != 0 {
                c.win_styles.clear();
                c.win_folds.clear();
                c.rope = Rope::from("");
                c.line_starts.clear();
                c.applied = None;
            }
            return;
        }
        let mut c = self.cache.borrow_mut();
        // ropey 实现了按内容的 PartialEq；`&c.rope != text` 不拷贝、不分配，
        // 文本未变（通知帧/重复更新）时保持已有产出，不重算。
        if &c.rope != text {
            c.reset_for(text.clone(), folding);
        }
    }

    /// 渲染时按**当前可见窗口**请求高亮；若缓存窗口不覆盖当前可见区，
    /// 就在本帧内**同步**重算（快速滚动至少色始终跟手，无跨线程滞后）。
    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let need = {
            let c = self.cache.borrow();
            let line_count = c.line_starts.len();
            if line_count == 0 {
                return resolve_styles(&c.win_styles, range, resolver);
            }
            let lo = line_for_byte(&c.line_starts, range.start);
            let hi = line_for_byte(&c.line_starts, range.end.saturating_sub(1));
            let req_lo = lo.min(line_count - 1);
            let req_hi = hi.min(line_count - 1);
            if requirement_met(line_count, c.applied, req_lo, req_hi) {
                None
            } else {
                Some((req_lo, req_hi))
            }
        };
        if let Some((req_lo, req_hi)) = need {
            self.compute_and_store(req_lo, req_hi);
        }
        let c = self.cache.borrow();
        resolve_styles(&c.win_styles, range, resolver)
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.cache.borrow().win_folds.clone()
    }
}

/// 创建可安装到编辑器的高亮工厂（找不到语法时返回 `None` = 不做高亮）。
///
/// Markdown 走本仓库手写的**逐行状态机**（[`markdown::MarkdownHighlighter`]，编辑只重扫
/// 受影响行，单键成本与文档总行数无关）；其余语言走 syntect 窗口式解析。
/// `tracker`：编辑侧通道，注入到每个高亮器实例（打字时记录 `InputEdit` 字节区间，
/// 供预览同步当 dirty 区间用）。
pub(crate) fn highlighter_factory_with_tracker(tracker: EditTracker) -> InputHighlighterFactory {
    Rc::new(move |language| {
        if normalize_language(language) == "markdown" {
            return Some(Box::new(markdown::MarkdownHighlighter::new(
                language, tracker.clone(),
            )) as Box<dyn InputHighlighter>);
        }
        SyntectHighlighter::new(language, tracker.clone())
            .map(|hl| Box::new(hl) as Box<dyn InputHighlighter>)
    })
}

/// 构建行首字节索引：`ret[i]` = 第 i 行的行首字节；末行结束 = `text.len()`。
fn build_line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.len() / 8 + 1);
    starts.push(0);
    for (i, &b) in text.as_bytes().iter().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// 从 ropey Rope 构建行首字节索引（行首调用 `lazy` 求字节偏移，O(行数)，
/// 不对全文做字符串复制）。
fn build_line_starts_rope(rope: &Rope) -> Vec<usize> {
    use gpui_component::input::RopeExt;
    let mut starts = Vec::with_capacity(rope.lines_len());
    for row in 0..rope.lines_len() {
        starts.push(rope.line_start_offset(row));
    }
    starts
}

/// 字节偏移 → 行号（0 基）。
fn line_for_byte(line_starts: &[usize], byte: usize) -> usize {
    match line_starts.binary_search(&byte) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

/// 解析可见窗口 `[req_lo, req_hi]`（含窗口前 [`LOOKBACK_LINES`] 行作状态前缀），
/// 产出**窗口字节范围内**的高亮段与折叠段（文档坐标）。纯函数，可后台线程执行。
fn compute_window(
    text: &str,
    line_starts: &[usize],
    req_lo: usize,
    req_hi: usize,
    language: &str,
    folding: bool,
) -> (Vec<(Range<usize>, &'static str)>, Vec<FoldRange>) {
    let line_count = line_starts.len();
    if line_count == 0 {
        return (Vec::new(), Vec::new());
    }
    let base = req_lo
        .saturating_sub(LOOKBACK_LINES)
        .min(line_count - 1);
    let hi = req_hi.min(line_count - 1).max(base);
    let start_byte = line_starts[base];
    let end_byte = if hi + 1 < line_count {
        line_starts[hi + 1]
    } else {
        text.len()
    };

    let syntax =
        find_syntax(language).unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut semantic_names: HashMap<Scope, Option<&'static str>> = HashMap::new();
    let mut highlights: Vec<(Range<usize>, &'static str)> = Vec::new();

    for li in base..=hi {
        let line_end = if li + 1 < line_count {
            line_starts[li + 1]
        } else {
            text.len()
        };
        let line = &text[line_starts[li]..line_end];
        if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
            let line_start = line_starts[li];
            let mut cursor = 0;
            for (index, operation) in operations {
                if cursor < index {
                    if let Some(name) = find_name(&mut semantic_names, &scopes) {
                        highlights.push((line_start + cursor..line_start + index, name));
                    }
                }
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            if cursor < line.len() {
                if let Some(name) = find_name(&mut semantic_names, &scopes) {
                    highlights.push((line_start + cursor..line_start + line.len(), name));
                }
            }
        }
    }

    // 只保留请求窗口内的段（lookback 前缀的段不属于当前窗口）
    let wlo = line_starts[req_lo.min(hi)];
    highlights.retain(|r| r.0.end > wlo && r.0.start < end_byte);

    let folds = if folding {
        brace_fold_ranges(&text[start_byte..end_byte], base)
    } else {
        Vec::new()
    };
    (highlights, folds)
}

/// 取 scope 链上最近的语义名（带缓存，避免重复字符串化 scope）。
fn find_name(
    cache: &mut HashMap<Scope, Option<&'static str>>,
    scopes: &ScopeStack,
) -> Option<&'static str> {
    scopes.scopes.iter().rev().find_map(|scope| {
        *cache.entry(*scope).or_insert_with(|| semantic_name(*scope))
    })
}

/// 把区间内的高亮段切成无缝隙的样式 run。`highlights` 有序且互不重叠，
/// 用二分定位首个候选段，避免每帧全量扫描。
pub(crate) fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let first = highlights.partition_point(|(highlight, _)| highlight.end <= range.start);
    let mut runs = Vec::new();
    let mut cursor = range.start;

    for (highlight_range, name) in &highlights[first..] {
        if highlight_range.start >= range.end {
            break;
        }

        let start = highlight_range.start.max(range.start);
        let end = highlight_range.end.min(range.end);
        if start >= end || end <= cursor {
            continue;
        }
        if cursor < start {
            runs.push((cursor..start, HighlightStyle::default()));
        }
        runs.push((start..end, resolver.style(name).unwrap_or_default()));
        cursor = end;
    }

    if cursor < range.end {
        runs.push((cursor..range.end, HighlightStyle::default()));
    }
    runs
}

/// Markdown 预览代码块的高亮结果：**每行一个**未解析的语义名 run
/// （行内字节区间 → 语义名）。与主题无关，可跨明/暗主题复用缓存。
pub(crate) type CodeLineRuns = Vec<Vec<(Range<usize>, &'static str)>>;

/// 解析整个代码块文本，返回每行的语义名 run（整块一次解析，
/// 多行注释 / 字符串 / 跨行语法上下文能正确配色，预览不会“逐行独立”地错色）。
/// `language` 无标记或未知 → 返回 `None`（按纯文本渲染，不产出高亮）。
///
/// TOML 不在 syntect 内置语法集里（syntect 5.3 的 default_newlines 无 TOML），
/// 走内置的轻量扫描器 [`toml_code_runs`]（与 GitHub 配色同一套语义名）。
pub(crate) fn parse_code_lines(language: &str, code: &str) -> Option<CodeLineRuns> {
    if normalize_language(language).eq_ignore_ascii_case("toml") {
        return Some(toml_code_runs(code));
    }
    let syntax = find_syntax(language)?;
    let line_starts = build_line_starts(code);
    let line_count = line_starts.len();
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut semantic_names: HashMap<Scope, Option<&'static str>> = HashMap::new();
    let mut out = Vec::with_capacity(line_count);
    for li in 0..line_count {
        let line_end = if li + 1 < line_count {
            line_starts[li + 1]
        } else {
            code.len()
        };
        let line = &code[line_starts[li]..line_end];
        let mut runs: Vec<(Range<usize>, &'static str)> = Vec::new();
        if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
            let mut cursor = 0;
            for (index, operation) in operations {
                if cursor < index {
                    if let Some(name) = find_name(&mut semantic_names, &scopes) {
                        runs.push((cursor..index, name));
                    }
                }
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            if cursor < line.len() {
                if let Some(name) = find_name(&mut semantic_names, &scopes) {
                    runs.push((cursor..line.len(), name));
                }
            }
        }
        out.push(runs);
    }
    Some(out)
}

/// TOML 字符串类型：basic `"..."` / literal `'...'` 与其三引号多行变体。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TomlStr {
    Basic,
    Literal,
    Basic3,
    Literal3,
}

impl TomlStr {
    /// 该字符串的行内定界符（单行 = 1 个引号，多行 = 3 个引号）。
    fn delim(&self) -> char {
        match self {
            TomlStr::Basic | TomlStr::Basic3 => '"',
            TomlStr::Literal | TomlStr::Literal3 => '\'',
        }
    }
    fn multiline(&self) -> bool {
        matches!(self, TomlStr::Basic3 | TomlStr::Literal3)
    }
    /// 多行字符串允许换行。
    fn is_literal(&self) -> bool {
        matches!(self, TomlStr::Literal | TomlStr::Literal3)
    }
}

/// 轻量 TOML 语法扫描器（syntect 内置语法集没有 TOML，这里手写一个按行扫描的
/// 语义分类器）。复用与 syntect 相同的语义名（comment/string/number/boolean/
/// property/function/operator…），因此直接接入 GitHub 明/暗配色。
///
/// 覆盖：`#` 注释、单/三引号字符串（支持跨行）、数值、布尔、裸/点分/引号键、
/// 表头 `[table]`、`=` 赋值。字符串内部查重、整体一次解析，跨行字符串不错位。
fn toml_code_runs(code: &str) -> CodeLineRuns {
    let line_starts = build_line_starts(code);
    let line_count = line_starts.len();
    let mut out = Vec::with_capacity(line_count);
    // 跨行字符串状态（None = 不在字符串内）；字符串跨行时在下一行从行首继续。
    let mut state: Option<TomlStr> = None;
    for li in 0..line_count {
        let line_end = if li + 1 < line_count {
            line_starts[li + 1]
        } else {
            code.len()
        };
        let line = &code[line_starts[li]..line_end];
        let (runs, next) = scan_toml_line(line, state);
        state = next;
        out.push(runs);
    }
    out
}

/// 扫描一行 TOML，返回该行的语义名 run 与退出时的跨行字符串状态。
/// `incoming`：进入本行时未闭合的字符串（在上一行已开始，本行从 0 处继续）。
fn scan_toml_line(line: &str, incoming: Option<TomlStr>) -> (Vec<(Range<usize>, &'static str)>, Option<TomlStr>) {
    let mut runs: Vec<(Range<usize>, &'static str)> = Vec::new();
    let mut i = 0usize;
    // 可见文本长度（去掉结尾换行/回车，避免把 '\n' 算进着色区间）
    let vis = line.trim_end_matches(['\n', '\r']).len();
    // 当前行内正在进行的字符串：Some((类型, 起始字节))（incoming → 从 0 继续）
    let mut cur: Option<(TomlStr, usize)> = incoming.map(|s| (s, 0));

    // —— section header：`[table]` / `[[array-of-tables]]` 整段染「function」色 ——
    // 判据：行首（可带缩进）为 `[`，首 `[` 到末 `]` 之间没有 `=`（避免把
    // `key = [1, 2]` 这类行误判成表头）。
    {
        let head = line.trim_start();
        if head.starts_with('[') {
            let open = line.len() - head.len();
            if let Some(rel) = head.rfind(']') {
                let close = open + rel;
                if !line[open + 1..close].contains('=') {
                    runs.push((open..close + 1, "function"));
                    i = close + 1;
                }
            }
        }
    }

    while i < line.len() {
        if let Some((kind, start)) = cur {
            // —— 处于字符串内：找闭合定界符 ——
            // 跨行续接时从行首(0)搜；本行新开时从开引号之后(i)搜。
            let search_from = if start == 0 && incoming.is_some() { 0 } else { i };
            match find_string_close(line, search_from, kind) {
                Some(close) => {
                    // 闭合 → 整段字符串；basic 型内的转义另拆成 string.escape
                    push_run(&mut runs, start, close, vis, "string");
                    if !kind.is_literal() {
                        escape_runs(line, start, close, &mut runs);
                    }
                    cur = None;
                    i = close;
                }
                None => {
                    // 未闭合 → 延续到本行末；着可见部分并保持跨行状态
                    push_run(&mut runs, start, vis, vis, "string");
                    if !kind.is_literal() {
                        escape_runs(line, start, vis, &mut runs);
                    }
                    i = line.len();
                }
            }
            continue;
        }

        let ch = line[i..].chars().next().expect("i < line.len()");
        match ch {
            '#' => {
                // 注释到行尾
                push_run(&mut runs, i, vis, vis, "comment");
                break;
            }
            '"' | '\'' => {
                // 字符串开头（含三引号检测）
                let (kind, next) = detect_string_kind(line, i);
                let start = i;
                cur = Some((kind, start));
                i = next;
            }
            '=' => {
                push_run(&mut runs, i, i + 1, vis, "operator");
                i += 1;
            }
            c if c.is_ascii_digit()
                || ((c == '+' || c == '-')
                    && line[i + c.len_utf8()..].chars().next().is_some_and(|n| n.is_ascii_digit())) =>
            {
                // 数值
                let (s, e) = scan_number(line, i, vis);
                push_run(&mut runs, s, e, vis, "number");
                i = e;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                // 标识符：布尔 / 键（后跟 `=` 的为 property）/ 其它（constant）
                let (s, e) = scan_word(line, i, vis);
                let word = &line[s..e];
                let name = if word == "true" || word == "false" {
                    "boolean"
                } else if lookahead_is_key(line, e, vis) {
                    "property"
                } else {
                    "constant"
                };
                push_run(&mut runs, s, e, vis, name);
                i = e;
            }
            _ => i += ch.len_utf8(), // 括号/逗号/点号等结构符不着色
        }
    }

    // 行尾仍开着字符串 → 跨行状态交给下一行（含空行续接多行字符串的情况）
    let outgoing = cur.map(|(kind, _)| kind);
    (runs, outgoing)
}

/// 压入着色 run（裁剪到可见区间 [0, vis]；空区间忽略）。
fn push_run(
    runs: &mut Vec<(Range<usize>, &'static str)>,
    s: usize,
    e: usize,
    vis: usize,
    name: &'static str,
) {
    let s = s.min(vis);
    let e = e.min(vis);
    if s < e {
        runs.push((s..e, name));
    }
}

/// 检测字符串开头：返回（类型，开引号之后的续位）。
fn detect_string_kind(line: &str, i: usize) -> (TomlStr, usize) {
    let ch = line[i..].chars().next().expect("ch");
    let triple = {
        let mut s = String::with_capacity(3);
        s.push(ch);
        s.push(ch);
        s.push(ch);
        s
    };
    let is_triple = line[i..].starts_with(&triple);
    match (ch, is_triple) {
        ('"', true) => (TomlStr::Basic3, i + 3),
        ('"', false) => (TomlStr::Basic, i + 1),
        ('\'', true) => (TomlStr::Literal3, i + 3),
        ('\'', false) => (TomlStr::Literal, i + 1),
        _ => unreachable!(),
    }
}

/// 从 `from` 起查找字符串闭合定界符（不含 from 处本身）。
/// - 单行：下一个（未转义的）同引号
/// - 三引号多行：下一组三个同引号（跨行搜索，含本行）
fn find_string_close(line: &str, from: usize, kind: TomlStr) -> Option<usize> {
    let q = kind.delim();
    if kind.multiline() {
        let slice = &line[from..];
        let pat = format!("{q}{q}{q}");
        let offset = slice.find(&pat)?;
        let idx = from + offset;
        // 跳过 `""""`（= 内容中的转义引号 + 闭合），取真正的三引号闭界
        // 简单起见：找到的 `"""` 即闭合（极少边界情况忽略）
        return Some(idx + 3);
    }
    // 单行：basic 型跳过 `\x`，literal 型无转义直接找
    if kind.is_literal() {
        return line[from..].find(q).map(|o| from + o + 1);
    }
    // basic 单行：`\"` 为转义，不入匹配
    let bytes = line.as_bytes();
    let mut pos = from;
    while pos < line.len() {
        let b = bytes[pos];
        if b == b'\\' && pos + 1 < line.len() {
            pos += 2;
            continue;
        }
        if b == b'"' {
            return Some(pos + 1);
        }
        pos += 1;
    }
    None
}

/// 把 `[start,end)` 内 basic 型字符串的 `\x` 转义切成 string.escape run。
fn escape_runs(
    line: &str,
    start: usize,
    end: usize,
    runs: &mut Vec<(Range<usize>, &'static str)>,
) {
    let bytes = line.as_bytes();
    let mut pos = start;
    while pos + 1 < end {
        if bytes[pos] == b'\\' {
            let esc_end = (pos + 2).min(end);
            runs.push((pos..esc_end, "string.escape"));
            pos = esc_end;
        } else {
            pos += 1;
        }
    }
}

/// 扫描数值 token：数字开头（或 ± 后跟数字）。吃进 [0-9A-Fa-fxob_+.eE]，外加 ±。
fn scan_number(line: &str, start: usize, vis: usize) -> (usize, usize) {
    let mut e = start;
    while e < vis {
        let c = line[e..].chars().next().unwrap();
        let c = c as u8; // 数值/符号均为 ASCII
        if c.is_ascii_hexdigit() || b"_+.eExob".contains(&c) {
            e += 1;
        } else {
            break;
        }
    }
    (start, e)
}

/// 扫描标识符 token：`[A-Za-z0-9_-]+` 或引号包裹的键。返回（起, 止）。
fn scan_word(line: &str, start: usize, vis: usize) -> (usize, usize) {
    let mut e = start;
    while e < vis {
        let c = line[e..].chars().next().unwrap();
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            e += c.len_utf8();
        } else {
            break;
        }
    }
    (start, e)
}

/// 向前窥探：跳过空格 / 点 / 更多的键段，若在遇到换行 / `,` / `]` / `}` 之前
/// 命中 `=` → 说明当前词是（点分）键 → true。
fn lookahead_is_key(line: &str, mut k: usize, vis: usize) -> bool {
    while k < vis {
        let ch = line[k..].chars().next().unwrap();
        match ch {
            c if c.is_whitespace() => k += c.len_utf8(),
            '.' => k += 1,
            '"' | '\'' => {
                // 引号键段：跳到同引号闭合之后
                let q = ch;
                match line[k + 1..].find(q) {
                    Some(off) => k += 1 + off + 1,
                    None => return false,
                }
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => {
                while k < vis {
                    let c2 = line[k..].chars().next().unwrap();
                    if c2.is_ascii_alphanumeric() || c2 == '_' || c2 == '-' {
                        k += c2.len_utf8();
                    } else {
                        break;
                    }
                }
            }
            '=' => return true,
            _ => return false,
        }
    }
    false
}

/// 把某一行的语义名 run 解析成**覆盖整行、无缝隙**的 `HighlightStyle` run。
/// 颜色由 `resolver`（GitHub 明/暗色板）决定；无命中段时返回单个默认 run（沿用外层文字色）。
/// 调用方应复用同一个 resolver 逐行解析，避免每行重建主题。
pub(crate) fn resolve_line_styles_with(
    runs: &[(Range<usize>, &'static str)],
    line_len: usize,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    resolve_styles(runs, &(0..line_len), resolver)
}

/// 把 syntect scope 字符串映射为语义名（与 `SyntaxColors::style` 的键一致）。
fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let name = if scope.starts_with("comment") {
        "comment"
    } else if scope.starts_with("constant.character.escape") {
        "string.escape"
    } else if scope.starts_with("string") {
        "string"
    } else if scope.starts_with("constant.numeric") {
        "number"
    } else if scope.starts_with("constant.language.boolean") {
        "boolean"
    } else if scope.starts_with("keyword.operator") {
        "operator"
    } else if scope.starts_with("keyword") || scope.starts_with("storage") {
        "keyword"
    } else if scope.starts_with("entity.name.function") || scope.starts_with("support.function") {
        "function"
    } else if scope.starts_with("entity.name.type")
        || scope.starts_with("entity.name.class")
        || scope.starts_with("support.type")
    {
        "type"
    } else if scope.starts_with("entity.name.tag") {
        "tag"
    } else if scope.starts_with("entity.other.attribute-name") {
        "attribute"
    } else if scope.starts_with("variable.other.property")
        || scope.starts_with("support.constant.property")
    {
        "property"
    } else if scope.starts_with("variable") {
        "variable"
    } else if scope.starts_with("constant") {
        "constant"
    } else if scope.starts_with("punctuation") {
        "punctuation"
    } else if scope.starts_with("entity.name.namespace") || scope.starts_with("keyword.control.import") {
        "preproc"
    } else {
        return None;
    };
    Some(name)
}

/// 花括号（`{}`）折叠范围：粗略按行配对，注释/字符串中被跳过。
/// `base_line`：文本切片起始的文档行号（折行号换算回文档坐标）。
fn brace_fold_ranges(text: &str, base_line: usize) -> Vec<FoldRange> {
    let mut starts = Vec::new();
    let mut ranges = Vec::new();

    for (line_number, line) in text.lines().enumerate() {
        let mut chars = line.chars().peekable();
        let mut quoted = false;
        let mut escaped = false;
        while let Some(character) = chars.next() {
            if !quoted && character == '/' && chars.peek() == Some(&'/') {
                break;
            }
            if character == '"' && !escaped {
                quoted = !quoted;
            } else if !quoted && character == '{' {
                starts.push(line_number);
            } else if !quoted && character == '}' {
                if let Some(start_line) = starts.pop() {
                    if start_line < line_number {
                        ranges.push(FoldRange::new(
                            base_line + start_line,
                            base_line + line_number,
                        ));
                    }
                }
            }
            escaped = quoted && character == '\\' && !escaped;
            if character != '\\' {
                escaped = false;
            }
        }
    }
    ranges
}

/// 构建单个 `ThemeStyle`（颜色 + 可选斜体）。
fn ts(color: u32, italic: bool) -> ThemeStyle {
    ThemeStyle {
        color: Some(gpui::rgb(color).into()),
        font_style: if italic { Some(FontStyle::Italic) } else { None },
        font_weight: None,
    }
}

/// GitHub 风格语法配色（明/暗两套），取自 GitHub 官方（Primer）语法色板。
/// 同时带上编辑区背景（暗色沿用应用当前的 0x111827）；行号栏不设独立背景色。
pub(crate) fn github_theme(dark: bool) -> HighlightTheme {
    let syntax = if dark {
        SyntaxColors {
            comment: Some(ts(0x8b949e, true)),
            comment_doc: Some(ts(0x8b949e, true)),
            keyword: Some(ts(0xff7b72, false)),
            string: Some(ts(0xa5d6ff, false)),
            string_escape: Some(ts(0x7ee787, false)),
            number: Some(ts(0x79c0ff, false)),
            constant: Some(ts(0x79c0ff, false)),
            boolean: Some(ts(0xff7b72, false)),
            operator: Some(ts(0xff7b72, false)),
            function: Some(ts(0xd2a8ff, false)),
            constructor: Some(ts(0xd2a8ff, false)),
            preproc: Some(ts(0x79c0ff, false)),
            property: Some(ts(0x79c0ff, false)),
            type_: Some(ts(0xffa657, false)),
            tag: Some(ts(0x7ee787, false)),
            attribute: Some(ts(0x79c0ff, false)),
            label: Some(ts(0xff7b72, false)),
            variant: Some(ts(0x79c0ff, false)),
            ..Default::default()
        }
    } else {
        SyntaxColors {
            comment: Some(ts(0x6a737d, true)),
            comment_doc: Some(ts(0x6a737d, true)),
            keyword: Some(ts(0xcf222e, false)),
            string: Some(ts(0x0a3069, false)),
            string_escape: Some(ts(0x116329, false)),
            number: Some(ts(0x0550ae, false)),
            constant: Some(ts(0x0550ae, false)),
            boolean: Some(ts(0xcf222e, false)),
            operator: Some(ts(0xcf222e, false)),
            function: Some(ts(0x8250df, false)),
            constructor: Some(ts(0x8250df, false)),
            preproc: Some(ts(0x0550ae, false)),
            property: Some(ts(0x0550ae, false)),
            type_: Some(ts(0x953800, false)),
            tag: Some(ts(0x116329, false)),
            attribute: Some(ts(0x0550ae, false)),
            label: Some(ts(0xcf222e, false)),
            variant: Some(ts(0x0550ae, false)),
            ..Default::default()
        }
    };

    HighlightTheme {
        name: if dark {
            "github-dark".into()
        } else {
            "github-light".into()
        },
        appearance: if dark {
            gpui_component::ThemeMode::Dark
        } else {
            gpui_component::ThemeMode::Light
        },
        style: HighlightThemeStyle {
            editor_background: Some(if dark {
                gpui::rgb(0x111827).into()
            } else {
                gpui::white()
            }),
            editor_foreground: Some(if dark {
                gpui::rgb(0xc9d1d9).into()
            } else {
                gpui::rgb(0x24292f).into()
            }),
            editor_active_line: None,
            editor_line_number: None,
            editor_active_line_number: None,
            editor_invisible: None,
            editor_gutter_background: None,
            status: StatusColors::default(),
            syntax,
        },
    }
}

/// 把 GitHub 语法配色装进当前主题（明/暗随 `Theme.mode`）。启动与切换主题后调用，
/// 因为 [`gpui_component::Theme::change`] 会用配置重置 `highlight_theme`。
pub(crate) fn install_github_theme(cx: &mut gpui::App) {
    let theme = gpui_component::Theme::global_mut(cx);
    theme.highlight_theme = Arc::new(github_theme(theme.mode.is_dark()));
}

/// 明/暗 GitHub 语法色板（各静态缓存一份）。代码块渲染**每帧**都要把逐行语义名
/// 解析成颜色（[`resolve_line_styles_with`]），若每次现建整套配色（~20 个
/// `ThemeStyle` 的堆分配 + 颜色转换）纯属浪费、随可见代码块数成倍放大。
/// 配色是编译期常量、只随明/暗变化，与运行时主题无关，静态缓存永久有效。
pub(crate) fn github_theme_cached(dark: bool) -> &'static HighlightTheme {
    static LIGHT: LazyLock<HighlightTheme> = LazyLock::new(|| github_theme(false));
    static DARK: LazyLock<HighlightTheme> = LazyLock::new(|| github_theme(true));
    if dark { &DARK } else { &LIGHT }
}

/// 项目代码块实现的**对外形状**：给围栏语言与块内文本（`\n` 连接、无尾换行），
/// 返回**块级字节偏移**、无缝隙的 [`HighlightStyle`] run —— 供 WYSIWYG live 的
/// `set_code_highlighter` 钩子使用，语义/配色与预览代码块完全一致
/// （同 `parse_code_lines` + `github_theme_cached`/`resolve_line_styles_with`）。
/// 未知语言/无标记 → 返回空（按纯文本渲染，与预览一致）。
pub(crate) fn block_highlights(
    language: &str,
    block: &str,
    dark: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(line_runs) = parse_code_lines(language, block) else {
        return Vec::new();
    };
    let resolver = github_theme_cached(dark);
    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut offset = 0usize;
    for (runs, line) in line_runs.iter().zip(block.split('\n')) {
        for (r, style) in resolve_line_styles_with(runs, line.len(), resolver) {
            out.push((r.start + offset..r.end + offset, style));
        }
        offset += line.len() + 1; // '\n'
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析器可覆盖常见语言；`resolve_styles` 把区间切成无缝隙的样式 run。
    #[test]
    fn syntax_covers_common_languages_and_styles_cover_range() {
        let tracker = EditTracker::default();
        for lang in ["rust", "python", "javascript", "html", "css", "json"] {
            let hl = SyntectHighlighter::new(lang, tracker.clone());
            let hl = hl.unwrap_or_else(|| panic!("{lang} 应在默认语法集内"));
            assert_eq!(hl.language(), lang);
        }
        assert!(SyntectHighlighter::new("no-such-lang-xyz", tracker).is_none());

        let resolver = github_theme(false);
        let styles =
            resolve_styles(&[(2..7, "function"), (18..25, "string")], &(2..25), &resolver);
        let ranges: Vec<Range<usize>> = styles.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![2..7, 7..18, 18..25], "样式段应无缝覆盖区间");
        assert_eq!(
            styles[0].1.color,
            Some(gpui::rgb(0x8250df).into()),
            "light 函数色应为 GitHub 紫 #8250df"
        );

        // 没有样式覆盖的区间 → 返回单个默认 run
        let empty = resolve_styles(&[], &(0..10), &resolver);
        assert_eq!(empty.len(), 1, "无命中时一个整段默认 run");
        assert_eq!(empty[0].0, 0..10);
    }

    #[test]
    fn normalize_language_aliases() {
        assert_eq!(normalize_language("sh"), "Shell Script");
        assert_eq!(normalize_language("yml"), "yaml");
        assert_eq!(normalize_language("ts"), "typescript");
        assert_eq!(normalize_language("rust"), "rust");
    }

    #[test]
    fn line_starts_and_byte_mapping() {
        let src = "ab\ncd\n最后一行无换行";
        let starts = build_line_starts(&src);
        assert_eq!(starts, vec![0, 3, 6]);
        assert_eq!(line_for_byte(&starts, 0), 0);
        assert_eq!(line_for_byte(&starts, 3), 1);
        assert_eq!(line_for_byte(&starts, 5), 1);
        assert_eq!(line_for_byte(&starts, src.len()), 2);
    }

    /// 窗口高亮：段只落在请求窗口的字节范围内，且能识别前序样式。
    #[test]
    fn window_highlights_only_visible_region() {
        let src = "// 顶部注释\nfn main() {\n    let s = \"hello\";\n}\n// 尾部注释\n";
        let starts = build_line_starts(&src);
        // 5 行内容 + 结尾换行生成的结束空行槽位
        assert_eq!(starts.len(), 6);
        assert_eq!(line_for_byte(&starts, 0), 0);

        // 请求中间窗口（行 1..3，函数体）：只产出窗口字节范围内的段
        let (hl, _) = compute_window(&src, &starts, 1, 3, "rust", false);
        assert!(!hl.is_empty(), "窗口内应有高亮段");
        let wlo = starts[1];
        let whi = starts[4];
        for (r, _) in &hl {
            assert!(
                r.start >= wlo && r.end <= whi,
                "高亮段应在窗口字节范围内: {r:?}"
            );
        }

        // 请求开头窗口：关键字 / 字符串 / 注释都应被识别
        let (hl2, _) = compute_window(&src, &starts, 0, 4, "rust", false);
        let mut names: Vec<_> = hl2.iter().map(|(_, n)| *n).collect();
        names.sort();
        assert!(names.contains(&"keyword"), "应识别 fn/let 等关键字");
        assert!(names.contains(&"string"), "应识别字符串");
        assert!(names.contains(&"comment"), "应识别行注释");
    }

    #[test]
    fn github_theme_has_explicit_light_and_dark_keyword_colors() {
        let light = github_theme(false);
        assert!(light.style.syntax.keyword.is_some(), "light 应有 keyword 色");
        let dark = github_theme(true);
        assert!(dark.style.syntax.keyword.is_some(), "dark 应有 keyword 色");
        assert_ne!(
            light.style.syntax.keyword.map(|s| s.color),
            dark.style.syntax.keyword.map(|s| s.color),
            "明暗 keyword 颜色应不同"
        );
    }

    /// 优化回归：`github_theme_cached` 必须**复用同一实例**（同主题两次调用返回同指针），
    /// 证明代码块渲染不再每帧重建整套配色；明/暗各一份互不串扰，检出不了则说明缓存失效。
    #[test]
    fn github_theme_cached_reuses_single_instance_per_theme() {
        let light1 = github_theme_cached(false) as *const HighlightTheme;
        let light2 = github_theme_cached(false) as *const HighlightTheme;
        let dark1 = github_theme_cached(true) as *const HighlightTheme;
        let dark2 = github_theme_cached(true) as *const HighlightTheme;
        assert!(std::ptr::eq(light1, light2), "同主题应复用同一缓存实例");
        assert!(std::ptr::eq(dark1, dark2), "同主题应复用同一缓存实例");
        assert!(!std::ptr::eq(light1, dark1), "明暗应为不同实例");
        // 缓存实例的颜色解析功能与现建一致（light function = #8250df）
        let styles = resolve_styles(
            &[(0..3, "function")],
            &(0..3),
            github_theme_cached(false),
        );
        assert_eq!(
            styles[0].1.color,
            Some(gpui::rgb(0x8250df).into()),
            "缓存色板应能正常解析颜色"
        );
    }

    /// 基准：GitHub 色板「每帧现建 vs 静态缓存」的代价对比（打印、不构成断言；
    /// `--ignored` 运行）。旧代码块渲染每帧 `github_theme(dark)` 重建整套配色，
    /// 新逻辑复用 [`github_theme_cached`] 的静态实例，仅做颜色解析。
    #[test]
    #[ignore]
    fn bench_github_theme_cached_vs_fresh() {
        use std::time::Instant;
        let runs = 20_000usize;
        let t0 = Instant::now();
        let mut sink = 0u64;
        for _ in 0..runs {
            let th = github_theme(true);
            sink = sink.wrapping_add(u64::from(th.style("keyword").is_some()));
        }
        let fresh = t0.elapsed();
        let t1 = Instant::now();
        let mut sink2 = 0u64;
        for _ in 0..runs {
            let th = github_theme_cached(true);
            sink2 = sink2.wrapping_add(u64::from(th.style("keyword").is_some()));
        }
        let cached = t1.elapsed();
        assert_eq!(sink, sink2, "缓存后解析结果必须一致");
        println!(
            "GitHub 色板解析（{runs} 次）：每帧现建 theme={}ms, 静态缓存={}ms（提升 {:.1}x）",
            fresh.as_millis(),
            cached.as_millis(),
            fresh.as_secs_f64() / cached.as_secs_f64().max(1e-9),
        );
    }

    /// 预览代码块高亮：整块解析 → 每行语义名 run；未知语言返回 None；
    /// `resolve_line_styles_with` 产出无缝隙、可着色、随明暗变化的 run。
    #[test]
    fn preview_code_block_highlight_lines() {
        // rust 多行注释/字符串跨行也能正确配色（无尾换行 → 行数与内容行一致）
        let code = "fn main() {\n    // 注释\n    let s = \"str\";\n}";
        let runs = parse_code_lines("rust", code).expect("rust 应有高亮");
        assert_eq!(runs.len(), 4, "每行一条");
        let names: Vec<_> = runs
            .iter()
            .map(|l| l.iter().map(|(_, n)| *n).collect::<Vec<_>>())
            .collect();
        assert!(names[0].contains(&"keyword"), "fn/关键字行应有 keyword: {names:?}");
        assert!(names[1].contains(&"comment"), "注释行应有 comment: {names:?}");
        assert!(names[2].contains(&"string"), "字符串行应有 string: {names:?}");

        // 未知语言 → None（不做高亮）
        assert!(parse_code_lines("no-such-lang", "x = 1").is_none());

        // 每行解析出的颜色 run 无缝隙覆盖整行（长度 = 行字节长）
        let resolver = github_theme(false);
        let mut total_colored = 0;
        for (li, line_runs) in runs.iter().enumerate() {
            let line = code.lines().nth(li).unwrap_or("");
            let styles = resolve_line_styles_with(line_runs, line.len(), &resolver);
            let mut cursor = 0;
            for (r, st) in &styles {
                assert_eq!(r.start, cursor, "行 {li} 应无缝隙");
                cursor = r.end;
                if st.color.is_some() {
                    total_colored += 1;
                }
            }
            assert_eq!(cursor, line.len(), "行 {li} 应覆盖整行");
        }
        assert!(total_colored > 0, "整块至少有颜色 run");
    }

    /// TOML 不在 syntect 内置语法集，走内置轻量扫描器：注释/表头/键/字符串/
    /// 数值/布尔/转义/跨行三引号字符串都能正确分类。
    #[test]
    fn preview_toml_code_block_highlight() {
        let code = "# 顶部注释\n[server]\nhost = \"127.0.0.1\"  # 行内注释\nport = 8080\ndebug = true\nesc = \"a\\nb\"\nname = 'literal'\n[server.deps]\nfeatures = [\"x\", 'y']\n";
        let runs = parse_code_lines("toml", code).expect("toml 应产出高亮");
        assert_eq!(runs.len(), 10, "9 内容行 + 末尾换行的空行槽位，每行一条");
        let names: Vec<Vec<&str>> = runs
            .iter()
            .map(|l| l.iter().map(|(_, n)| *n).collect())
            .collect();
        let flat: Vec<&str> = names.iter().flatten().copied().collect();
        for want in ["comment", "function", "property", "string", "number", "boolean", "string.escape", "operator"] {
            assert!(
                flat.contains(&want),
                "toml 应含有 {want} 高亮，实际 names={flat:?}"
            );
        }
        // 表头 `[server]` 整段为 function；键 `host` 为 property
        assert!(names[1].contains(&"function"), "表头行应是 function: {:?}", names[1]);
        assert!(names[2].contains(&"property"), "键行应是 property: {:?}", names[2]);
        assert!(names[2].contains(&"comment"), "行内注释应识别: {:?}", names[2]);

        // 跨行三引号基本字符串：整段连续着色为 string（状态跨行）
        let multi = "s = \"\"\"\n第一行\n第二行\"\"\"\n";
        let runs = parse_code_lines("toml", multi).unwrap();
        assert_eq!(runs.len(), 4, "3 内容行 + 末尾换行的空行槽位");
        let flat: Vec<&str> = runs.iter().flat_map(|l| l.iter().map(|(_, n)| *n)).collect();
        let strings = flat.iter().filter(|n| **n == "string").count();
        assert!(
            strings >= 3,
            "三引号字符串内容跨行应全部着色为 string: {flat:?}"
        );
    }

    /// 基准：窗口解析代价与文档总量无关（打印、不构成断言；`--ignored` 运行）。
    #[test]
    #[ignore]
    fn bench_window_highlight_cost() {
        use std::time::Instant;
        let block: String = (0..10)
            .map(|i| {
                format!(
                    "fn func_{i}(a: i32, b: &str) -> Result<(), Box<dyn Error>> {{\n    // 注释注释注释\n    let s = \"string literal 字符串\";\n    if a > {i} {{ println!(\"{{}}\", s); }} else {{ return Err(\"e\".into()); }}\n    Ok(())\n}}\n"
                )
            })
            .collect();
        // 构造约 16 万行的大文档（与“大文档测试.rs”同量级）
        let mut src = String::new();
        for _ in 0..1600 {
            src.push_str(&block);
        }
        let line_count = build_line_starts(&src).len();
        println!("文档 {} MB / {} 行", src.len() / 1024 / 1024, line_count);
        let starts = build_line_starts(&src);
        for (name, lo) in [
            ("head", 0usize),
            ("mid", line_count / 2 - 500),
            ("deep", line_count - 2000),
        ] {
            let t = Instant::now();
            let (hl, _) = compute_window(&src, &starts, lo, lo + 40, "rust", true);
            println!("{name}: window@{lo} = {}ms, {} 段", t.elapsed().as_millis(), hl.len());
        }
    }

    /// 回归：`compute_window_doc` 的产物必须落在**文档坐标**——
    /// 高亮字节段受 `start_byte` 平移、折叠行号受 `parse_lo` 平移，
    /// 而不是窗口内的小偏移（漏平移会让中部/末尾窗口的高亮与折叠坐标错乱）。
    #[test]
    fn window_doc_produces_document_coordinates() {
        use gpui_component::input::RopeExt;

        // ~1000 行、含注释/字符串/花括号的 rust 文档
        let mut src = String::new();
        for i in 0..200 {
            src.push_str(&format!(
                "// 注释 {i}\nfn func_{i}() {{\n    let s = \"str\";\n    // 内\n}}\n"
            ));
        }
        let rope = Rope::from(src.as_str());
        let line_starts = build_line_starts_rope(&rope);
        assert_eq!(line_starts.len(), rope.lines_len(), "行索引应与 rope 行数一致");

        // 请求中后部窗口：若坐标没平移，产物会落在窗口内的极小值
        let req_lo = 500usize;
        let req_hi = 505usize;
        let Some(((styles, folds), plo, _phi)) =
            compute_window_doc(&rope, &line_starts, req_lo, req_hi, "rust", true)
        else {
            panic!("中部窗口应可解析");
        };
        assert!(!styles.is_empty(), "应有高亮段");
        let plo_byte = line_starts[plo];
        assert!(
            styles.iter().all(|(r, _)| r.start >= plo_byte && r.end <= rope.len()),
            "高亮段应为文档字节坐标 >= 窗口起始字节 {plo_byte}: {styles:?}"
        );
        // 至少一段落在请求窗口内
        let req_byte = line_starts[req_lo];
        assert!(
            styles.iter().any(|(r, _)| r.start >= req_byte),
            "应有高亮段落在请求窗口内: {styles:?}"
        );

        assert!(!folds.is_empty(), "应有折叠段");
        // 漏平移的折叠行号 ≤ 窗口高度(~116)；平移后 ≈ 文档行号(≥400)
        assert!(
            folds.iter().all(|f| f.start_line >= req_lo.saturating_sub(100) && f.end_line <= rope.lines_len()),
            "折叠行号应为文档坐标(带 parse_lo 平移): {folds:?}"
        );
    }

    /// `block_highlights`（WYSIWYG live 的代码块对接）：块级偏移、覆盖每行、
    /// 未知语言返回空——与预览代码块同语义。
    #[test]
    fn block_highlights_covers_block_and_ignores_unknown_lang() {
        let code = "let a = 1;\nlet b = 2;\n";
        let hl = block_highlights("rust", code, false);
        assert!(!hl.is_empty(), "rust 块应有高亮");
        // 覆盖 = 每行行长之和（`\n` 间隙不覆盖，live 按行裁剪）
        let nl = code.matches('\n').count();
        let covered: usize = hl.iter().map(|(r, _)| r.end - r.start).sum();
        assert_eq!(covered, code.len() - nl, "应覆盖整块文本（去换行）");
        // 块级偏移都在边界内、且按序不重叠
        let mut prev = 0usize;
        for (r, _) in &hl {
            assert!(r.start >= prev, "应有序不重叠: {hl:?}");
            assert!(r.end <= code.len(), "超出块边界: {hl:?}");
            prev = r.end;
        }
        // 多行注释上下文能跨行（与预览一致：整块解析，不只逐行独立）
        let comment = "/* 注释\n跨两行 */\nlet x = 1;\n";
        assert!(
            !block_highlights("rust", comment, false).is_empty(),
            "跨行注释块应有高亮（说明按整块解析，非逐行独立）"
        );
        // 未知语言 → 空（纯文本）
        assert!(block_highlights("definitely_not_a_lang", "x", false).is_empty());
        assert!(block_highlights("", "x", false).is_empty());
    }
}
