//! `![alt](src)` 识别的共享核心（预览 `parser` 与 WYSIWYG `live` 共用）。
//!
//! 两套管线都从行首解析 `![alt](src)`，但各自叠加不同策略（整行/`{width=N}`/列表前缀），
//! 且对 `![a]b](c)` 这类「裸 `]` 出现在 `](` 之前」的边界语义不同——差异已由
//! `markdown::parity::image_construct_edges_are_documented` 钉死。本模块只共享
//! **前缀识别**（`![` + `]` 紧跟 `(` + src 非空），策略留各调用方。

/// `![alt](src)` 前缀识别结果（各字段都借用输入）。
pub(crate) struct ImgRef<'a> {
    /// alt（原样，未 trim；live 不用它）。
    pub(crate) alt: &'a str,
    /// src（已 trim）。
    pub(crate) src: &'a str,
    /// `)` 之后的尾部（未 trim）。
    pub(crate) tail: &'a str,
    /// `](` 中 `]` 相对 `![` 之后的偏移（供 parser 精确复刻其「首个 `]`」历史规则）。
    pub(crate) close_alt_rel: usize,
}

/// 从行首解析 `![alt](src)`：要求 `![` 后第一个 `](` 成对、`(` 后有 `)`、src 非空。
/// 只做前缀匹配；整行 / `{width=N}` / 列表前缀等策略由调用方决定（见 parity 差异清单）。
pub(crate) fn parse_ref(line: &str) -> Option<ImgRef<'_>> {
    let rest = line.strip_prefix("![")?;
    let close_alt = rest.find("](")?;
    let alt = &rest[..close_alt];
    let after = &rest[close_alt + 2..];
    let close_src = after.find(')')?;
    let src = after[..close_src].trim();
    let tail = &after[close_src + 1..];
    (!src.is_empty()).then_some(ImgRef {
        alt,
        src,
        tail,
        close_alt_rel: close_alt,
    })
}
