//! 窗口式语法高亮：只对「可见行窗口（+前瞻回看）」跑 syntect 解析。
//!
//! 设计吸收 **gpui-component/element.rs 的窗口式高亮** 与本仓库 `src/syntax` 的
//! `WindowCache`：解析量大但限定在可见窗口（多少行就解析多少），并**按
//! (`buffer.version`, 行窗口) 缓存**；滚动复用、编辑过期。搜索/滚动时最坏即可见窗口
//! 重解析，与文档总大小无关。
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

/// 共享语法集（默认语法集几 MB，进程级共享一次）。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// 语言名 → 语法解析结果缓存（`find_syntax_by_token` 全量扫描较慢）。
static SYNTAX_CACHE: LazyLock<Mutex<HashMap<String, Option<&'static SyntaxReference>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let lang = language.to_owned();
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

/// 高亮器：持有语言语法 + 「版本×行窗口」缓存。
pub struct Highlighter {
    syntax: Option<&'static SyntaxReference>,
    dark: bool,
    /// 缓存键：`(buffer.version, 解析起点行, 解析终点行)`。
    key: Option<(u64, usize, usize)>,
    /// 解析起始行（含回看前缀）。
    base: usize,
    /// 逐行样式（相对每行文本字节）。
    lines: Vec<Vec<(Range<usize>, Hsla)>>,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            syntax: None,
            dark: false,
            key: None,
            base: 0,
            lines: Vec::new(),
        }
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
            self.key = None;
        }
    }

    /// 切换明/暗色板（拖主题时调用）。
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            self.key = None;
        }
    }

    /// 请求可见行窗口 `[lo, hi]`（含两端）的高亮。返回与窗口对齐的每行样式。
    /// 命中缓存直接返回；否则以 `lookback` 行回看起点重新解析窗口。
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
            self.base = 0;
            self.key = None;
            return &self.lines;
        }
        let lo = lo.min(line_count - 1);
        let hi = hi.min(line_count - 1);
        let version = buffer.version();

        if let Some((v, _a, b)) = self.key {
            if v == version && lo >= self.base && hi < self.base + self.lines.len() {
                let rel = lo - self.base;
                if rel + (hi - lo) + 1 <= self.lines.len() {
                    return &self.lines[rel..rel + (hi - lo) + 1];
                }
            }
            let _ = b;
        }

        // 重解析：从回看起点到 hi。
        let start = lo.saturating_sub(lookback).min(line_count.saturating_sub(1));
        let mut lines: Vec<Vec<(Range<usize>, Hsla)>> = Vec::with_capacity(hi - start + 1);
        let Some(syntax) = self.syntax else {
            for _ in start..=hi {
                lines.push(Vec::new());
            }
            self.lines = lines;
            self.base = start;
            self.key = Some((version, lo, hi));
            return &self.lines;
        };

        let dark = self.dark;
        let mut parser = ParseState::new(syntax);
        for row in start..=hi {
            let text = buffer.line_text(row);
            let mut runs: Vec<(Range<usize>, Hsla)> = Vec::new();
            self.parse_line(&mut parser, text.as_ref(), &mut runs, dark);
            lines.push(runs);
        }
        self.lines = lines;
        self.base = start;
        self.key = Some((version, lo, hi));
        &self.lines
    }

    /// 解析起点行（回看前缀起点；调用方用 `row - base` 访问 `lines`）。
    pub fn base(&self) -> usize {
        self.base
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
            if let Some(name) = name {
                if let Some(color) = name_color(name, dark) {
                    out.push((from..to, color));
                }
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
