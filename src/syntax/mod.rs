//! 编辑器代码高亮：syntect 语法解析 + GitHub 风格配色。
//!
//! gpui-component 的 CodeEditor 是**解析无关**的：只要提供实现
//! [`gpui_component::input::InputHighlighter`] 的解析器（这里用 syntect 的默认语法集）
//! 与一个 [`gpui_component::highlighter::HighlightTheme`] 配色（GitHub Light/Dark），
//! 编辑区即可着色。组件自带的 tree-sitter 工厂在 `tree-sitter` 特性关闭时是空操作，
//! 因此由本模块负责提供真实的语法解析与配色。
//!
//! - [`highlighter_factory`]：创建编辑器（`app::tabs::make_tab`）时安装
//! - [`install_github_theme`]：启动（`main.rs`）与切换明/暗主题（`app::toggle_theme`）时安装

use std::{
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
use syntect::{
    parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

/// 共享语法集（默认语法定义有几 MB，跨实例共享一次）。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

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

/// 按语言 token 或扩展名定位语法；找不到返回 `None`（不产出高亮）。
fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let lang = normalize_language(language);
    SYNTAX_SET
        .find_syntax_by_token(lang)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(lang))
}

/// 基于 syntect 的整篇重解析适配器（无增量编辑，但结构简单、覆盖语言广）。
/// 产出「语义名 → 字节区间」列表，配合解析器无关的 [`HighlightStyleResolver`]
/// （即 GitHub 配色的 `SyntaxColors`）上色。
struct SyntectHighlighter {
    language: SharedString,
    /// 有序、不重叠的语义高亮段（区间 → 语义名）。
    highlights: Vec<(Range<usize>, &'static str)>,
    fold_ranges: Vec<FoldRange>,
    /// Scope id → 语义名缓存（scope 字符串化较贵）。
    semantic_names: HashMap<Scope, Option<&'static str>>,
}

impl SyntectHighlighter {
    fn new(language: &str) -> Option<Self> {
        find_syntax(language)?;
        Some(Self {
            language: language.to_owned().into(),
            highlights: Vec::new(),
            fold_ranges: Vec::new(),
            semantic_names: HashMap::new(),
        })
    }

    fn push_highlight(&mut self, range: Range<usize>, scopes: &ScopeStack) {
        if range.is_empty() {
            return;
        }
        let name = scopes.scopes.iter().rev().find_map(|scope| {
            *self
                .semantic_names
                .entry(*scope)
                .or_insert_with(|| semantic_name(*scope))
        });
        if let Some(name) = name {
            self.highlights.push((range, name));
        }
    }
}

impl InputHighlighter for SyntectHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        // 整篇重解析（syntect 无增量模式）。读出 rope 一次，折叠复用同一份文本。
        let text = text.to_string();
        let syntax =
            find_syntax(self.language.as_ref()).unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        let mut parser = ParseState::new(syntax);
        let mut scopes = ScopeStack::new();
        let mut offset = 0;
        self.highlights.clear();

        for line in LinesWithEndings::from(&text) {
            if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
                let mut cursor = 0;
                for (index, operation) in operations {
                    self.push_highlight(offset + cursor..offset + index, &scopes);
                    let _ = scopes.apply(&operation);
                    cursor = index;
                }
                self.push_highlight(offset + cursor..offset + line.len(), &scopes);
            }
            offset += line.len();
        }

        self.fold_ranges = if folding {
            brace_fold_ranges(&text)
        } else {
            Vec::new()
        };
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        resolve_styles(&self.highlights, range, resolver)
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.fold_ranges.clone()
    }
}

/// 创建可安装到编辑器的高亮工厂（找不到语法时返回 `None` = 不做高亮）。
pub(crate) fn highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        SyntectHighlighter::new(language)
            .map(|hl| Box::new(hl) as Box<dyn InputHighlighter>)
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
    } else if scope.starts_with("entity.name.namespace")
        || scope.starts_with("keyword.control.import")
    {
        "preproc"
    } else {
        return None;
    };
    Some(name)
}

/// 花括号（`{}`）折叠范围：粗略按行配对，注释/字符串中被跳过。
fn brace_fold_ranges(text: &str) -> Vec<FoldRange> {
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
                        ranges.push(FoldRange::new(start_line, line_number));
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

    /// 解析出的语义名应存在对应语法；`resolve_styles` 应把目标区间切成无缝隙的样式 run。
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
}
