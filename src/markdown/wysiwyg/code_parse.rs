use gpui::SharedString;

/// 代码块高亮解析缓存键：`(语言名, 代码全文)`。`parse_code_lines` 是纯计算
/// （与预览共用全局 `SYNTAX_SET`/`SYNTAX_CACHE`，不依赖 window/平台）→ 可在
/// background executor 跑；就绪后 `on_code_parse_ready` **定向作废该代码块的整形缓存**
/// （不参与全局 shape_key）→ 该块按下帧重建用高亮行替换占位纯文本行，其余块保持命中
/// （对齐 `math_gen` 只影响本身块的语义，避免代码块内每键打字把整带缓存全量 miss）。
pub(crate) type CodeParseKey = (SharedString, SharedString);

/// [`CodeParseKey`] 对应的缓存条目（后台线程写、主线程读）。
#[derive(Clone)]
pub(crate) enum CodeParseState {
    /// 解析在途（防重复发起）。
    Pending,
    /// 已解析；`None` = 该语言无高亮（按纯文本渲染）。
    Done(Option<crate::syntax::CodeLineRuns>),
}

/// [`WyEditor::code_parse_lookup`] 的返回（shape 期用）。
pub(crate) enum CodeParseLookup {
    /// 已就绪：直接用其着色（`None` = 纯文本）。
    Ready(Option<crate::syntax::CodeLineRuns>),
    /// 未就绪：登记后台解析，本帧先按纯文本渲染（就绪后自动上色）。
    NotReady,
}
