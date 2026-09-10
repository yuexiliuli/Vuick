//! 编辑器代码高亮：syntect 语法解析 + GitHub 风格配色，**窗口式高亮**。
//!
//! gpui-component 的 CodeEditor 是**解析无关**的：只要提供实现
//! [`gpui_component::input::InputHighlighter`] 的解析器（这里用 syntect 的默认语法集）
//! 与一个 [`gpui_component::highlighter::HighlightTheme`] 配色（GitHub Light/Dark），
//! 编辑区即可着色。组件自带的 tree-sitter 工厂在 `tree-sitter` 特性关闭时是空操作，
//! 因此由本模块负责提供真实的语法解析与配色。
//!
//! ## 性能设计（窗口式，任意文档大小都有高亮且不卡 UI）
//!
//! syntect 整篇解析在 debug 构建下很慢（约 50µs/行），而编辑器**每次编辑**都会触发
//! [`InputHighlighter::update`]，因此**不整篇解析**：只在后台解析**当前可见窗口**
//! （可见行 + 上下过扫 + 窗口前一段 [`LOOKBACK_LINES`] 行作状态前缀，代价有界），
//! 一次最多 ~600 行 ≈ 30ms，与文档总大小无关——6MB / 16 万行文档同样即时配色。
//! 滚动时窗口请求变化 → 后台重算 → 通知重绘，颜色延迟一瞬即到。
//!
//! - [`highlighter_factory`]：创建编辑器（`app::tabs::make_tab`）时安装
//! - [`install_github_theme`]：启动（`main.rs`）与切换明/暗主题（`app::toggle_theme`）时安装

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    rc::Rc,
    sync::{Arc, LazyLock},
};

use gpui::{Context, HighlightStyle, SharedString, Task, Window};
use gpui_component::highlighter::{
    FontStyle, HighlightTheme, HighlightThemeStyle, StatusColors, SyntaxColors, ThemeStyle,
};
use gpui_component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// 超过该体积的文档不做高亮（工作集过大，无法支撑换代场景；正常文档远小于此）。
const MAX_TEXT_BYTES: usize = 24 * 1024 * 1024;
/// 可见窗口**向前**多解析的行数（语法状态前缀），使跨越行边界的注释/字符串/代码块
/// 在大多数情况下也能正确配色。解析一次窗口 ≈ (窗口 + 2×WINDOW_PAD + LOOKBACK) 行，
/// 优化后的 syntect 约 0.25ms/行 → 单次窗口重算几十毫秒，与文档总大小无关。
const LOOKBACK_LINES: usize = 128;
/// 计算时向可见窗口上下各扩展的行数：滚动在余量内不触发重算，减少抖动。
const WINDOW_PAD: usize = 128;
/// 后台工作线程的空闲/繁忙轮询间隔。
const IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(150);
const BUSY_TICK: std::time::Duration = std::time::Duration::from_millis(16);

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
    /// 当前文本快照（`update` 时整份复制，用于判断文本是否变化）。
    text: SharedString,
    /// 行 i 的行首字节偏移；`line_starts.len()` = 行数（末行结束 = `text.len()`）。
    line_starts: Vec<usize>,
    /// 折叠使能（当前文本）。
    folding: bool,
    /// 可见窗口行区间（请求，由 [`InputHighlighter::styles`] 更新）。
    req_lo: usize,
    req_hi: usize,
    /// 已产出对应的 (req_lo, req_hi)；与当前请求一致则无需重算。
    applied: Option<(usize, usize)>,
    /// 窗口行内的高亮段（文档字节坐标 → 语义名）。
    win_styles: Vec<(Range<usize>, &'static str)>,
    /// 窗口内的折叠段（行号基于文档）。
    win_folds: Vec<FoldRange>,
}

impl WindowCache {
    fn new() -> Self {
        Self {
            text: SharedString::default(),
            line_starts: Vec::new(),
            folding: false,
            req_lo: 0,
            req_hi: 0,
            applied: None,
            win_styles: Vec::new(),
            win_folds: Vec::new(),
        }
    }

    /// 文本变化时重建行索引并清空产出，让后台线按新文本重算顶部窗口。
    fn reset_for(&mut self, text: String, folding: bool) {
        let line_starts = build_line_starts(&text);
        self.text = text.into();
        self.line_starts = line_starts;
        self.folding = folding;
        self.req_lo = 0;
        self.req_hi = 0;
        self.applied = None;
        self.win_styles.clear();
        self.win_folds.clear();
    }
}

/// 活跃的 [`InputHighlighter`]：持有一个长期存活的窗口高亮后台任务
/// （见 [`Self::ensure_worker`]）。文本变更通过 [`WindowCache::reset_for`] 通知任务。
struct SyntectHighlighter {
    language: SharedString,
    cache: Rc<RefCell<WindowCache>>,
    /// 关闭标记：高亮器被丢弃（关标签页/换文件）时置位，后台线随之退出。
    closed: Rc<Cell<bool>>,
    worker_started: Rc<Cell<bool>>,
    worker: Rc<RefCell<Option<Task<()>>>>,
}

impl SyntectHighlighter {
    fn new(language: &str) -> Option<Self> {
        find_syntax(language)?;
        Some(Self {
            language: language.to_owned().into(),
            cache: Rc::new(RefCell::new(WindowCache::new())),
            closed: Rc::new(Cell::new(false)),
            worker_started: Rc::new(Cell::new(false)),
            worker: Rc::new(RefCell::new(None)),
        })
    }

    /// 启动（仅一次）窗口高亮后台任务：轮询请求 → 后台解析可见窗口 → 写回 → 通知重绘。
    fn ensure_worker(&mut self, window: &mut Window, cx: &mut Context<EditorState>) {
        if self.worker_started.replace(true) {
            return;
        }
        let cache = self.cache.clone();
        let closed = self.closed.clone();
        let language = self.language.clone();
        let task = cx.spawn_in(window, async move |entity, cx| {
            loop {
                if closed.get() {
                    break;
                }
                // 读取请求与文本快照（主线程）
                let (text, starts, line_count, req_lo, req_hi, applied, folding) = {
                    let c = cache.borrow();
                    (
                        c.text.clone(),
                        c.line_starts.clone(),
                        c.line_starts.len(),
                        c.req_lo,
                        c.req_hi,
                        c.applied,
                        c.folding,
                    )
                };
                if text.is_empty() || requirement_met(line_count, applied, req_lo, req_hi) {
                    cx.background_executor().timer(IDLE_TICK).await;
                    continue;
                }
                // 向可见窗口上下扩展 WINDOW_PAD 行：滚动在该余量内不触发重算
                let padded_lo = req_lo.saturating_sub(WINDOW_PAD);
                let padded_hi = (req_hi + WINDOW_PAD).min(line_count.saturating_sub(1));
                // 后台解析可见窗口（含 lookback 前缀），代价有界、与文档总大小无关
                let txt2 = text.clone();
                let lang2 = language.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        compute_window(
                            txt2.as_ref(),
                            &starts,
                            padded_lo,
                            padded_hi,
                            lang2.as_ref(),
                            folding,
                        )
                    })
                    .await;
                let (styles, folds) = result;
                {
                    let mut c = cache.borrow_mut();
                    // 若期间文本已更新：本次结果作废（下一轮按新快照重算）
                    if c.text.as_ref() == text.as_str() {
                        c.win_styles = styles;
                        c.win_folds = folds;
                        c.applied = Some((padded_lo, padded_hi));
                    }
                }
                let _ = entity.update(cx, |_state, cx| {
                    cx.notify();
                });
                cx.background_executor().timer(BUSY_TICK).await;
            }
        });
        *self.worker.borrow_mut() = Some(task);
    }
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

impl Drop for SyntectHighlighter {
    fn drop(&mut self) {
        self.closed.set(true);
    }
}

impl InputHighlighter for SyntectHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    /// 每次编辑/加载都会调用：只重建行索引并登记文本，**不做同步解析**（不卡 UI）。
    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        folding: bool,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if text.len() > MAX_TEXT_BYTES {
            // 超大纲：清掉高亮（保持即时响应），后台线无事可做成空闲
            let mut c = self.cache.borrow_mut();
            if !c.text.is_empty() {
                c.win_styles.clear();
                c.win_folds.clear();
                c.text = SharedString::default();
                c.line_starts.clear();
                c.applied = None;
                cx.notify();
            }
            return;
        }
        let t = text.to_string();
        {
            let mut c = self.cache.borrow_mut();
            if c.text.as_ref() != t.as_str() {
                c.reset_for(t, folding);
            }
            // 文本未变（通知帧/重复更新）：保持已有产出，不重算
        }
        self.ensure_worker(window, cx);
    }

    /// 渲染时按**当前可见窗口**请求高亮；返回已产出的窗口高亮（未就绪则无色，
    /// 后台算好后通知重绘）。
    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut c = self.cache.borrow_mut();
        let line_count = c.line_starts.len();
        if line_count > 0 {
            let lo = line_for_byte(&c.line_starts, range.start);
            let hi = line_for_byte(&c.line_starts, range.end.saturating_sub(1));
            c.req_lo = lo.min(line_count - 1);
            c.req_hi = hi.min(line_count - 1);
        }
        resolve_styles(&c.win_styles, range, resolver)
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.cache.borrow().win_folds.clone()
    }
}

/// 创建可安装到编辑器的高亮工厂（找不到语法时返回 `None` = 不做高亮）。
pub(crate) fn highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        SyntectHighlighter::new(language)
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
fn resolve_styles(
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
/// 同时带上编辑区/行号栏背景（暗色沿用应用当前的 0x111827）。
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
            editor_gutter_background: Some(if dark {
                gpui::rgb(0x111827).into()
            } else {
                gpui::rgb(0xf6f8fa).into()
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析器可覆盖常见语言；`resolve_styles` 把区间切成无缝隙的样式 run。
    #[test]
    fn syntax_covers_common_languages_and_styles_cover_range() {
        for lang in ["rust", "python", "javascript", "html", "css", "json"] {
            let hl = SyntectHighlighter::new(lang);
            let hl = hl.unwrap_or_else(|| panic!("{lang} 应在默认语法集内"));
            assert_eq!(hl.language(), lang);
        }
        assert!(SyntectHighlighter::new("no-such-lang-xyz").is_none());

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
}
