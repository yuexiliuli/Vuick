//! 代码块语法高亮（#12 收编）：syntect 默认语法集 + GitHub（Primer）明/暗配色。
//!
//! 只保留**被预览与 WYSIWYG 真实使用**的子集：
//! - [`parse_code_lines`]：整块解析出每行的语义名 run（`CodeLineRuns`）；
//! - [`github_theme`] / [`github_theme_cached`]：明/暗 GitHub 配色 → [`HighlightStyle`]；
//! - [`resolve_styles`] / [`resolve_line_styles_with`]：语义名 → 样式 run；
//! - [`prewarm`]：启动预热（语法集 + 常用语言 + 明暗色板）。
//!
//! **不依赖 gpui_component**：配色是用 [`GitHubTheme`]（自研，实现本地
//! [`SemanticStyleResolver`]）替代组件库 `HighlightTheme`/`HighlightStyleResolver`；
//! 组件库的 `InputHighlighter` 接入已被自研编辑器替代（死代码，已删）。

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use gpui::{HighlightStyle, FontStyle};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// 预热生命周期标记：`prewarm_begin`（启动路径把 `prewarm` 派到后台线程**前**同步置位，
/// 消除「会话恢复先于预热任务运行」的竞态）→ `prewarm` 后台跑完置 `PREWARM_DONE`。
/// 消费方（shell 会话恢复等）在 `started && !done` 期间让帧等待，保证打开文件时
/// `find_syntax` 全部命中缓存、主线程零冷扫（syntax crate 注释承认首次认证 ~200-250ms）。
static PREWARM_STARTED: AtomicBool = AtomicBool::new(false);
static PREWARM_DONE: AtomicBool = AtomicBool::new(false);

/// 标记「预热已启动」（二进制壳 main.rs 在把 `prewarm` spawn 到后台前同步调用）。
pub fn prewarm_begin() {
    PREWARM_STARTED.store(true, Ordering::Release);
}

/// 预热是否已启动（含完成）。未启动 = 本进程不走预热路径（如测试进程）→ 不应等待。
pub fn prewarm_started() -> bool {
    PREWARM_STARTED.load(Ordering::Acquire)
}

/// 预热是否已完成：完成后 `find_syntax` 全部命中缓存，主线程打开文件零阻塞。
pub fn prewarm_done() -> bool {
    PREWARM_DONE.load(Ordering::Acquire)
}

/// 全局语法集（Windows syntax-based detection）；进程内仅加载一次。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// 进程级共享语法集引用：editor 窗口高亮与预览/WYSIWYG 块高亮共用同一实例
/// （单一真值，不再各自 `load_defaults_newlines` 一份）。
pub fn syntax_set() -> &'static SyntaxSet {
    &SYNTAX_SET
}

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
/// editor 窗口高亮跨 crate 消费（单真值；丢失的 alias 归一由本函数统一收编）。
pub fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let lang = normalize_language(language).to_owned();
    let key = lang.to_lowercase();
    if let Ok(cache) = SYNTAX_CACHE.lock()
        && let Some(r) = cache.get(&key) {
            return *r;
        }
    let r = SYNTAX_SET
        .find_syntax_by_token(&lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(&lang));
    if let Ok(mut cache) = SYNTAX_CACHE.lock() {
        cache.insert(key, r);
    }
    r
}

/// 把默认语法集的**全部**语言名与文件扩展名（小写）预填进 [`SYNTAX_CACHE`]：
/// 之后任意围栏语言只要是已知语法名或扩展名，`find_syntax` 直接命中缓存，不再触发
/// `find_syntax_by_token` 的全量扫描——该扫描**每次语言首次调用约 200-250ms**
/// （debug 实测 rust 250ms / python 200ms / json 7ms），是首个代码块渲染/滚动冻结的
/// 主要来源。一次性建表成本与单次扫描相当，放在启动后台线程（[`prewarm`]）。
fn prefill_syntax_cache() {
    if let Ok(mut cache) = SYNTAX_CACHE.lock() {
        for s in SYNTAX_SET.syntaxes() {
            cache.insert(s.name.to_lowercase(), Some(s));
            for ext in &s.file_extensions {
                cache.insert(ext.to_lowercase(), Some(s));
            }
        }
    }
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

/// 取 scope 链上最近的语义名（带缓存，避免重复字符串化 scope）。
fn find_name(
    cache: &mut HashMap<Scope, Option<&'static str>>,
    scopes: &ScopeStack,
) -> Option<&'static str> {
    scopes.scopes.iter().rev().find_map(|scope| {
        semantic_name_cached(cache, *scope)
    })
}

/// 把 syntect scope 字符串映射为语义名（与 [`GitHubTheme::style`] 的键一致）。
pub fn semantic_name(scope: Scope) -> Option<&'static str> {
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

/// 带 per-scope 缓存的语义名映射：调用方跨行/跨段复用同一 `cache`，避免每段都
/// `scope.build_string()` 字符串化。映射真值仍单一在 [`semantic_name`]。
pub fn semantic_name_cached(
    cache: &mut HashMap<Scope, Option<&'static str>>,
    scope: Scope,
) -> Option<&'static str> {
    *cache.entry(scope).or_insert_with(|| semantic_name(scope))
}

// -------------------------------------------------------------------------
// 配色：GitHub（Primer）明/暗两套，替代 gpui_component 的 HighlightTheme
// -------------------------------------------------------------------------

/// 语义名 → 样式解析器（本地 trait，替代组件库 `HighlightStyleResolver`）。
pub trait SemanticStyleResolver {
    fn style(&self, name: &str) -> Option<HighlightStyle>;
}

/// 明/暗 GitHub 语法配色（编译期常量；`style` 按语义名查色）。
/// 未设色（`variable`/`punctuation` 等）→ None → 沿用外层文字色。
pub struct GitHubTheme {
    dark: bool,
}

fn ts_hs(color: u32, italic: bool) -> HighlightStyle {
    HighlightStyle {
        color: Some(gpui::rgb(color).into()),
        font_style: if italic { Some(FontStyle::Italic) } else { None },
        ..Default::default()
    }
}

impl SemanticStyleResolver for GitHubTheme {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        if name.is_empty() {
            return None;
        }
        // 与组件库 `SyntaxColors::style` 一致：前缀回退（如 `string.escape` 未命中也
        // 按首段 `string`）——本项目语义名已知，逐一映射即可，无需回退。
        let (color, italic) = if self.dark {
            match name {
                "comment" | "comment.doc" => (0x8b949e, true),
                "string.escape" => (0x7ee787, false),
                "string" => (0xa5d6ff, false),
                "number" => (0x79c0ff, false),
                "boolean" => (0xff7b72, false),
                "operator" => (0xff7b72, false),
                "keyword" => (0xff7b72, false),
                "function" | "constructor" => (0xd2a8ff, false),
                "type" => (0xffa657, false),
                "tag" => (0x7ee787, false),
                "attribute" => (0x79c0ff, false),
                "property" => (0x79c0ff, false),
                "constant" => (0x79c0ff, false),
                "preproc" => (0x79c0ff, false),
                "label" => (0xff7b72, false),
                "variant" => (0x79c0ff, false),
                _ => return None,
            }
        } else {
            match name {
                "comment" | "comment.doc" => (0x6a737d, true),
                "string.escape" => (0x116329, false),
                "string" => (0x0a3069, false),
                "number" => (0x0550ae, false),
                "boolean" => (0xcf222e, false),
                "operator" => (0xcf222e, false),
                "keyword" => (0xcf222e, false),
                "function" | "constructor" => (0x8250df, false),
                "type" => (0x953800, false),
                "tag" => (0x116329, false),
                "attribute" => (0x0550ae, false),
                "property" => (0x0550ae, false),
                "constant" => (0x0550ae, false),
                "preproc" => (0x0550ae, false),
                "label" => (0xcf222e, false),
                "variant" => (0x0550ae, false),
                _ => return None,
            }
        };
        Some(ts_hs(color, italic))
    }
}

/// GitHub 风格语法配色（明/暗两套），取自 GitHub 官方（Primer）语法色板。
pub fn github_theme(dark: bool) -> GitHubTheme {
    GitHubTheme { dark }
}

/// 明/暗 GitHub 语法色板（各静态缓存一份）。代码块渲染**每帧**都要把逐行语义名
/// 解析成颜色（[`resolve_line_styles_with`]），静态缓存避免每帧重建配色。
pub fn github_theme_cached(dark: bool) -> &'static GitHubTheme {
    static LIGHT: LazyLock<GitHubTheme> = LazyLock::new(|| github_theme(false));
    static DARK: LazyLock<GitHubTheme> = LazyLock::new(|| github_theme(true));
    if dark { &DARK } else { &LIGHT }
}

// -------------------------------------------------------------------------
// 语义名 run → 样式 run
// -------------------------------------------------------------------------

/// 把区间内的高亮段切成无缝隙的样式 run。`highlights` 有序且互不重叠，
/// 用二分定位首个候选段，避免每帧全量扫描。
pub fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn SemanticStyleResolver,
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

/// 把某一行的语义名 run 解析成**覆盖整行、无缝隙**的 `HighlightStyle` run。
/// 颜色由 `resolver`（GitHub 明/暗色板）决定；无命中段时返回单个默认 run（沿用外层文字色）。
/// 调用方应复用同一个 resolver 逐行解析，避免每行重建主题。
pub fn resolve_line_styles_with(
    runs: &[(Range<usize>, &'static str)],
    line_len: usize,
    resolver: &dyn SemanticStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    resolve_styles(runs, &(0..line_len), resolver)
}

// -------------------------------------------------------------------------
// 整块解析：每行语义名 run
// -------------------------------------------------------------------------

/// Markdown 预览代码块的高亮结果：**每行一个**未解析的语义名 run
/// （行内字节区间 → 语义名）。与主题无关，可跨明/暗主题复用缓存。
pub type CodeLineRuns = Vec<Vec<(Range<usize>, &'static str)>>;

/// 解析整个代码块文本，返回每行的语义名 run（整块一次解析，
/// 多行注释 / 字符串 / 跨行语法上下文能正确配色，预览不会“逐行独立”地错色）。
/// `language` 无标记或未知 → 返回 `None`（按纯文本渲染，不产出高亮）。
///
/// TOML 不在 syntect 内置语法集里（syntect 5.3 的 default_newlines 无 TOML），
/// 走内置的轻量扫描器 [`toml_code_runs`]（与 GitHub 配色同一套语义名）。
pub fn parse_code_lines(language: &str, code: &str) -> Option<CodeLineRuns> {
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
                if cursor < index
                    && let Some(name) = find_name(&mut semantic_names, &scopes) {
                        runs.push((cursor..index, name));
                    }
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            if cursor < line.len()
                && let Some(name) = find_name(&mut semantic_names, &scopes) {
                    runs.push((cursor..line.len(), name));
                }
        }
        out.push(runs);
    }
    Some(out)
}

/// TOML 字符串类型：basic `"..."` / literal `'...'` 与其三引号多行变体。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    // 行尾仍开着字符串 → 仅多行型（三引号 Basic3/Literal3）才跨行续接；
    // 单行型未闭合是 TOML 非法，不透出 outgoing，否则下一行被误染成字符串续行（P1-32）。
    let outgoing = match cur {
        Some((kind, _)) if kind.multiline() => Some(kind),
        _ => None,
    };
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
/// 反斜杠字节 0x5C 不是 UTF-8 续字节/首字节，故逐字节步进只会命中真实反斜杠；
/// 转义目标按整字符长度推进，防 `\你` 用 `pos+2` 切中多字节 → 非边界 range。
fn escape_runs(
    line: &str,
    start: usize,
    end: usize,
    runs: &mut Vec<(Range<usize>, &'static str)>,
) {
    let mut pos = start;
    while pos + 1 < end {
        if line.as_bytes()[pos] == b'\\' {
            // 转义吃进下一整字符（按字符边界），`\x` 覆盖 1 字节反斜杠 + 整字符
            let nxt = line[pos + 1..end]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            let esc_end = (pos + 1 + nxt).min(end);
            runs.push((pos..esc_end, "string.escape"));
            pos = esc_end;
        } else {
            pos += 1;
        }
    }
}

/// 扫描数值 token：数字开头（或 ± 后跟数字）。吃进 [0-9A-Fa-fxob_+.eE]，外加 ±。
/// 用 char 判定而非 `c as u8` 截断：多字节字符低字节命中 hexdigit 时（如
/// `1ı`，U+0131 低字节 0x31=‘1’），截断判法会 `e+=1` 越字符边界 → `line[e..]`
/// 非边界切片 panic（P1-31）。
fn scan_number(line: &str, start: usize, vis: usize) -> (usize, usize) {
    let mut e = start;
    while e < vis {
        let c = line[e..].chars().next().unwrap();
        if c.is_ascii_hexdigit() || (c.is_ascii() && b"_+.eExob".contains(&(c as u8))) {
            e += c.len_utf8();
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

// -------------------------------------------------------------------------
// 对外：预热 + 块级高亮（WYSIWYG live）
// -------------------------------------------------------------------------

/// 启动期后台预热（A2）：语法高亮的一次性同步成本从「首次滚到带语言代码块」的
/// 那一帧挪到启动后台线程——
/// - `SYNTAX_SET`：`SyntaxSet::load_defaults_newlines` 反序列化几 MB 默认语法集；
/// - `SYNTAX_CACHE`：预填常用语言 → 首个代码块不再对全集做首次 `find_syntax` 扫描；
/// - `github_theme_cached`：明/暗两张色板各一份，首次上色不再现场构建主题。
///   `LazyLock`/`Mutex` 并发安全：可在后台线程调用，主线程若抢先命中只会短暂阻塞。
pub fn prewarm() {
    // 触发全局语法集加载（进程内仅一次）。
    let _ = &*SYNTAX_SET;
    // 预填常用语言映射；未知/无命中（如 js 走扩展名）返回 None，无害。
    for lang in [
        "rust", "sh", "bash", "python", "json", "yaml", "c", "cpp", "go", "typescript",
    ] {
        let _ = find_syntax(lang);
    }
    // 全量预填：所有语法名+扩展名 → 缓存，首个任意语言代码块不再冷扫语法集
    // （`find_syntax_by_token` ~200-250ms/语言，见 `prefill_syntax_cache`）。
    prefill_syntax_cache();
    // 明暗 GitHub 色板。
    let _ = github_theme_cached(false);
    let _ = github_theme_cached(true);
    // 全部就绪：find_syntax 此后全部命中缓存（会话恢复/打开文件不再有主线程冷扫）。
    PREWARM_DONE.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机诊断（`#[ignore]`）：首次 `find_syntax` 的冷成本拆解——`SYNTAX_SET`
    /// `load_defaults_newlines` 反序列化几 MB + `find_syntax_by_token` 全量扫描
    /// （debug 注释实测 rust ~250ms）。**若该冷加载发生在 UI 线程的
    /// `set_language(Some("rust"))` 且后台 prewarm 未完成，用户开 .rs 就会先白屏冻结**
    /// ——文本文件从不 set_language，故只有 .rs 有「先空白后加载」。
    /// 跑法：`cargo test -p syntax bench_cold_first_find_syntax -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_cold_first_find_syntax() {
        let t0 = std::time::Instant::now();
        let r = find_syntax("rust");
        let t1 = std::time::Instant::now();
        let r2 = find_syntax("rust");
        let t2 = std::time::Instant::now();
        let warm_set = std::time::Instant::now();
        let _ = &*SYNTAX_SET;
        let t3 = std::time::Instant::now();
        assert!(r.is_some() && r2.is_some());
        eprintln!(
            "[BENCH] cold find_syntax(rust)（SyntaxSet 反序列化 + 首扫）: {:.1} ms；warm: {:.1} ms；SYNTAX_SET 就绪后取引用: {:.3} ms",
            (t1 - t0).as_secs_f64() * 1000.0,
            (t2 - t1).as_secs_f64() * 1000.0,
            (t3 - warm_set).as_secs_f64() * 1000.0,
        );
    }

    /// 预热生命周期标记（A2）：`prewarm_begin` → 预热中/完成；完成后 `prewarm_done()`
    /// 为 true（会话恢复据此判定「find_syntax 已全量命中缓存」）。
    #[test]
    fn prewarm_lifecycle_flags() {
        prewarm_begin();
        assert!(prewarm_started(), "prewarm_begin 后应进入「已启动」态");
        prewarm();
        assert!(prewarm_done(), "prewarm 完成后应置 done");
    }

    /// 解析器可覆盖常见语言；`resolve_styles` 把区间切成无缝隙的样式 run。
    #[test]
    fn syntax_covers_common_languages_and_styles_cover_range() {
        for lang in ["rust", "python", "javascript", "html", "css", "json"] {
            assert!(
                parse_code_lines(lang, "fn main() {}\n").is_some(),
                "{lang} 应在默认语法集内"
            );
        }
        assert!(parse_code_lines("no-such-lang-xyz", "hi").is_none());

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
        let starts = build_line_starts(src);
        assert_eq!(starts, vec![0, 3, 6]);
        assert_eq!(parse_code_lines("rust", "fn main() {}\n").map(|r| r.len()), Some(2));
    }

    /// 语义名边界：可识别关键字 / 字符串 / 注释（活路径 parse_code_lines）。
    #[test]
    fn parse_code_lines_marks_keywords_strings_comments() {
        let src = "// 顶部注释\nfn main() {\n    let s = \"hello\";\n}\n";
        let runs = parse_code_lines("rust", src).expect("rust 解析应成功");
        let mut names: Vec<_> = runs
            .iter()
            .flatten()
            .map(|(_, n)| *n)
            .collect();
        names.sort();
        assert!(names.contains(&"comment"), "应识别行注释");
        assert!(names.contains(&"string"), "应识别字符串");
        assert!(names.contains(&"keyword"), "应识别 fn/let 关键字");
    }

    /// 优化回归：`github_theme_cached` 必须**复用同一实例**（同主题两次调用返回同指针），
    /// 证明代码块渲染不再每帧重建整套配色；明/暗各一份互不串扰，检出不了则说明缓存失效。
    #[test]
    fn github_theme_cached_reuses_single_instance_per_theme() {
        let light1 = github_theme_cached(false) as *const GitHubTheme;
        let light2 = github_theme_cached(false) as *const GitHubTheme;
        let dark1 = github_theme_cached(true) as *const GitHubTheme;
        let dark2 = github_theme_cached(true) as *const GitHubTheme;
        assert!(std::ptr::eq(light1, light2), "同主题应复用同一缓存实例");
        assert!(std::ptr::eq(dark1, dark2), "同主题应复用同一缓存实例");
        assert!(!std::ptr::eq(light1, dark1), "明暗应为不同实例");
        // 缓存实例的颜色解析功能与现建一致（light function = #8250df）
        let styles = resolve_styles(
            &[(0..3, "function")],
            &(0..3),
            github_theme_cached(false),
        );
        assert_eq!(styles[0].1.color, Some(gpui::rgb(0x8250df).into()));
        assert_eq!(
            github_theme(false).style("comment").map(|s| s.font_style),
            github_theme_cached(false).style("comment").map(|s| s.font_style),
            "现建与缓存实例的斜体标记一致"
        );
    }

    // —— 自研 TOML 行扫描器直测（评审 P2 可测性：多字节 / 空行 / 转义 / 跨行边界）——

    #[test]
    fn toml_section_header_and_comment() {
        let (runs, out) = scan_toml_line("[server]\n", None);
        assert!(out.is_none());
        assert_eq!(runs, vec![(0..8, "function")], "表头整段染 function");
        // 注释到行尾：未闭合引号不进入字符串状态
        let (runs, out) = scan_toml_line("# 注释 \"x", None);
        assert!(out.is_none());
        assert_eq!(runs, vec![(0..11, "comment")], "注释含引号也只是 comment");
    }

    #[test]
    fn toml_key_value_number_and_operator() {
        let (runs, _) = scan_toml_line("port = 8080x\n", None);
        let mut names: Vec<&str> = runs.iter().map(|(_, n)| *n).collect();
        names.sort();
        assert_eq!(names, vec!["number", "operator", "property"], "键=数值着色");
    }

    #[test]
    fn toml_string_escaped_quote_closes_at_real_end() {
        // `"a\"b"` 的 `\"` 是转义，闭合点应为行内最后一个引号
        let src = r#"x = "a\"b""#;
        let (runs, out) = scan_toml_line(src, None);
        assert!(out.is_none(), "转义串应在本行闭合");
        assert!(
            runs.iter().any(|(r, n)| *n == "string" && r.start == 4 && r.end == 10),
            "string run 覆盖整串（含闭合引号）"
        );
        assert!(runs.iter().any(|(_, n)| *n == "string.escape"), "转义拆出 escape");
    }

    #[test]
    fn toml_multiline_string_carries_and_closes_state() {
        // 三引号未闭合 → 跨行状态传给下一行
        let (runs, out) = scan_toml_line("s = \"\"\"a\n", None);
        assert_eq!(out, Some(TomlStr::Basic3), "未闭合三引号 → outgoing 状态");
        assert!(runs.iter().any(|(_, n)| *n == "string"), "未闭合串也着色可见部分");
        // 下一行续接并闭合（含跨行边界）
        let (runs2, out2) = scan_toml_line("b\n\"\"\"\n", out);
        assert_eq!(out2, None, "续接行应闭合三引号");
        assert!(runs2.iter().any(|(_, n)| *n == "string"), "续接段也着色");
    }

    /// P1-32 红测：TOML 单行字符串不允许跨行——本行 `s = \"abc` 未闭合，
    /// outgoing 必须透 None，下一行不能被染成字符串续行。
    #[test]
    fn toml_unclosed_single_line_string_does_not_leak_to_next_line() {
        let (runs1, out1) = scan_toml_line("s = \"abc", None);
        assert!(
            runs1.iter().any(|(_, n)| *n == "string"),
            "本行未闭合处仍染 string（本行恢复着色）"
        );
        assert_eq!(out1, None, "单行字符串未闭合 → 不透出 outgoing（不跨行）");
        let (runs2, _) = scan_toml_line("x = 5", out1);
        assert!(
            !runs2.iter().any(|(_, n)| *n == "string"),
            "下一行不应被当字符串续行"
        );
        assert!(
            runs2.iter().any(|(_, n)| *n == "number"),
            "下一行照常是 键=数值"
        );
    }

    #[test]
    fn toml_scanner_multibyte_and_empty_boundaries_do_not_panic() {
        // 多字节字符在数字/标识符后：停在字符边界、绝无越界 panic
        assert_eq!(scan_number("3代", 0, 3), (0, 1), "数字后接多字节 → 停在边界");
        assert_eq!(scan_word("abc中文", 0, 5), (0, 3), "标识符后接多字节 → 停在边界");
        assert_eq!(scan_number("0x1F", 0, 4), (0, 4), "十六进制全吞");
        assert_eq!(scan_number("1e10z", 0, 5), (0, 4), "非十六进制字母停住");
        // 空行 / 仅换行 / 行尾无换行
        assert!(scan_toml_line("", None).0.is_empty());
        assert!(scan_toml_line("\r\n", None).0.is_empty(), "换行不落入着色");
        let (runs, _) = scan_toml_line("tail = 1", None); // 无尾换行
        assert!(runs.iter().any(|(_, n)| *n == "number"));
    }

    /// P1-31 红测：数值扫描不得用 `c as u8` 截断判 hexdigit——`ı`(U+0131) 低字节
    /// 0x31=`'1'` 旧码会 `e+=1` 越字符边界，下一轮 `line[e..]` 切片 panic。
    #[test]
    fn toml_number_truncation_low_byte_hexdigit_does_not_panic() {
        assert_eq!(scan_number("1ı", 0, 3), (0, 1), "1ı：U+0131 低字节 0x31 命中 hexdigit，必须停在字符边界");
        assert_eq!(scan_number("1代", 0, 4), (0, 1), "数字后接多字节 → 停在边界");
        // 顶层 TOML 解析也不 panic，且 `ı` 不被染成 number（仅 `1` 是 number）
        let runs = toml_code_runs("x = 1ı");
        assert_eq!(runs.len(), 1, "单行输出");
        let names: Vec<&str> = runs[0].iter().map(|(_, n)| *n).collect();
        assert_eq!(names, vec!["property", "operator", "number"], "1ı 中仅 1 认作 number");
    }

    /// P1-31 附带防御：basic 串内 `\你` 的转义 run 必须以整字符边界终止，
    /// 防 `pos+2` 切中多字节 → 下游按 range 切片 panic。
    #[test]
    fn toml_escape_run_boundary_survives_multibyte_target() {
        let line = "s = \"\\你\"";
        let (runs, out) = scan_toml_line(line, None);
        assert!(out.is_none(), "转义串应在本行闭合");
        // 全部 run 起止均须在字符边界上（下游按 range 切片不 panic）
        for (r, _) in &runs {
            assert!(line.get(r.clone()).is_some(), "run {r:?} 必须在字符边界上");
        }
        let esc: Vec<Range<usize>> = runs
            .iter()
            .filter(|(_, n)| *n == "string.escape")
            .map(|(r, _)| r.clone())
            .collect();
        assert_eq!(esc, vec![5..9], "\\你 转义 run 覆盖 \\ 与整个 你（4 字节）");
    }

    #[test]
    fn toml_lookahead_distinguishes_key_from_constant() {
        assert!(lookahead_is_key("foo =", 3, 5), "词后 `=` → 键");
        assert!(lookahead_is_key("foo.bar = 1", 3, 11), "点分键 → 键");
        assert!(!lookahead_is_key("foo, bar", 3, 9), "`,` → 非键");
        assert!(!lookahead_is_key("foo", 3, 3), "到行尾无 `=` → 非键");
        assert!(lookahead_is_key("a.\"k\" = 1", 2, 11), "引号键段 → 键");
    }

    #[test]
    fn toml_find_string_close_handles_escaping() {
        assert_eq!(find_string_close(r#""a\"b""#, 1, TomlStr::Basic), Some(6), "basic 串要跳过转义引号");
        assert_eq!(find_string_close("'ab'x", 1, TomlStr::Literal), Some(4), "literal 串不转义");
        assert_eq!(find_string_close("\"abc", 1, TomlStr::Basic), None, "未闭合 → None");
    }

    #[test]
    fn toml_detect_string_kind_kinds() {
        assert_eq!(detect_string_kind("\"x", 0).0, TomlStr::Basic);
        assert_eq!(detect_string_kind("\"\"\"x", 0).0, TomlStr::Basic3);
        assert_eq!(detect_string_kind("'x", 0).0, TomlStr::Literal);
        assert_eq!(detect_string_kind("'''x", 0).0, TomlStr::Literal3);
    }
}
