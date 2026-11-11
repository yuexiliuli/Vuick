//! 文件分类（模块级单一真值，2026-11-11 自 `crates/shell` 的 `app/preview.rs` 迁出）：
//! `classify` 把「扩展名 → 类型」与「语言 id / 状态栏 label」三张手写表合为一处；
//! `is_*` 判定与 `language_id` / `language_label` 全部改调它。纯函数，无宿主依赖。

use std::path::Path;

/// 文件类型分类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    Markdown,
    Epub,
    Pdf,
    Image,
    Video,
    /// 代码 / 语言文件（语法高亮 id + 状态栏 label 内联，见 [`PathKind::language_id`]）。
    Code { language_id: &'static str, language_label: &'static str },
    /// 纯文本（`.txt`）。
    Text,
    /// 未知扩展名。
    Other,
}

impl PathKind {
    /// 语法高亮语言 id（未知扩展名回退 "text"）。
    pub fn language_id(self) -> &'static str {
        match self {
            PathKind::Markdown => "markdown",
            PathKind::Code { language_id, .. } => language_id,
            _ => "text",
        }
    }

    /// 状态栏语言标签（未知扩展名回退 "文本"）。
    pub fn language_label(self) -> &'static str {
        match self {
            PathKind::Markdown => "Markdown",
            PathKind::Epub => "EPUB",
            PathKind::Pdf => "PDF",
            PathKind::Image => "图片",
            PathKind::Code { language_label, .. } => language_label,
            PathKind::Text => "纯文本",
            _ => "文本",
        }
    }
}

/// 按扩展名分类文件（大小写不敏感）。图片/视频检测委托各自查看器白名单
/// （`image_viewer::IMAGE_EXTENSIONS` / `video::VIDEO_EXTENSIONS`，那里仍是是否存在
/// 该媒体的唯一来源；本表只依据结果给 [`PathKind`] 与 label）。
pub fn classify(p: &Path) -> PathKind {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "md" | "markdown" => PathKind::Markdown,
        "epub" => PathKind::Epub,
        "pdf" => PathKind::Pdf,
        "txt" => PathKind::Text,
        _ if image_viewer::is_image_extension(p) => PathKind::Image,
        #[cfg(feature = "video")]
        _ if video::is_video_extension(p) => PathKind::Video,
        "rs" => PathKind::Code { language_id: "rust", language_label: "Rust" },
        "toml" => PathKind::Code { language_id: "toml", language_label: "TOML" },
        "json" => PathKind::Code { language_id: "json", language_label: "JSON" },
        "py" => PathKind::Code { language_id: "python", language_label: "Python" },
        "js" | "jsx" => PathKind::Code { language_id: "javascript", language_label: "JavaScript" },
        "ts" | "tsx" => PathKind::Code { language_id: "typescript", language_label: "JavaScript" },
        "html" | "htm" => PathKind::Code { language_id: "html", language_label: "HTML" },
        "css" | "scss" => PathKind::Code { language_id: "css", language_label: "CSS" },
        "c" | "h" => PathKind::Code { language_id: "c", language_label: "C" },
        "cpp" | "cc" | "hpp" => PathKind::Code { language_id: "cpp", language_label: "C++" },
        "go" => PathKind::Code { language_id: "go", language_label: "Go" },
        "java" => PathKind::Code { language_id: "java", language_label: "Java" },
        "sh" | "bash" => PathKind::Code { language_id: "bash", language_label: "Shell" },
        "yaml" | "yml" => PathKind::Code { language_id: "yaml", language_label: "YAML" },
        "xml" => PathKind::Code { language_id: "text", language_label: "XML" },
        "sql" => PathKind::Code { language_id: "sql", language_label: "SQL" },
        _ => PathKind::Other,
    }
}

/// 是否 Markdown 文件（编辑区/预览默认换行的依据）。
pub fn is_markdown_path(p: &Path) -> bool {
    classify(p) == PathKind::Markdown
}

/// 是否 EPUB 文件（按扩展名识别；EPUB 预览走 WyEditor 只读宿主的依据）。
pub fn is_epub_path(p: &Path) -> bool {
    classify(p) == PathKind::Epub
}

/// 是否 PDF 文件（按扩展名识别）。
pub fn is_pdf_path(p: &Path) -> bool {
    classify(p) == PathKind::Pdf
}

/// 是否图片文件（按扩展名识别；清单见 [`image_viewer::IMAGE_EXTENSIONS`]）。
pub fn is_image_path(p: &Path) -> bool {
    classify(p) == PathKind::Image
}

/// 是否视频文件（按扩展名识别；清单见 [`video::VIDEO_EXTENSIONS`]）。
#[cfg(feature = "video")]
pub fn is_video_path(p: &Path) -> bool {
    classify(p) == PathKind::Video
}

/// 按扩展名推断 gpui-component 语法高亮的语言名（未知扩展名回退 "text"）。
pub fn language_id(p: &Path) -> &'static str {
    classify(p).language_id()
}

/// 选项卡是否 Markdown 文件（编辑区/预览默认换行的依据；读模型路径）。
pub fn is_markdown_tab(m: &shell_core::AppModel, idx: usize) -> bool {
    m.tabs
        .get(idx)
        .and_then(|t| t.file_path.as_ref())
        .map(|p| is_markdown_path(p))
        .unwrap_or(false)
}

/// 选项卡是否 EPUB 文件（按扩展名识别；EPUB 预览走 WyEditor 只读宿主的依据）。
pub fn is_epub_tab(m: &shell_core::AppModel, idx: usize) -> bool {
    m.tabs
        .get(idx)
        .and_then(|t| t.file_path.as_ref())
        .map(|p| is_epub_path(p))
        .unwrap_or(false)
}
