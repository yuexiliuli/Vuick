//! `Doc` 的源行 ↔ 字节辅助（自宿主 `wysiwyg/edit.rs`/`core.rs` 的 `WyEditor::row_col`/
//! `line_start`/`line_end`/`line_text` 迁入引擎的纯函数形态）：`shape`/`frame`/`paint`/
//! `hit` 迁移后不再持有 `&WyEditor`，统一经 `host.doc()` + 本模块换算。函数体与宿主
//! 原方法逐字一致（宿主内联方法保留，供宿主自带路径使用；两处对拍由编译期同义保证）。

use markdown::doc::Doc;

/// 源字节偏移 → (源行, 行内字节偏移)。用 `Doc::line_starts` 二分（O(log n)）。
pub fn row_col(doc: &Doc, off: usize) -> (usize, usize) {
    let off = off.min(doc.len());
    let ls = doc.line_starts();
    let line = ls.partition_point(|&st| st <= off).saturating_sub(1);
    let start = ls.get(line).unwrap_or(0);
    (line, off - start)
}

/// 源行 → 该行起始字节（O(log n)，`line_starts` 二分）。
pub fn line_start(doc: &Doc, line: usize) -> usize {
    doc.line_starts().get(line).unwrap_or(doc.len())
}

/// 源行 → 行尾字节（不含换行符；`line_starts[line+1]-1` = 行尾 `\n` 位置）。
/// 末行（无换行）返回内容尾。`CRLF` 下返回 `\n`（含 `\r`，与原文语义一致）。
pub fn line_end(doc: &Doc, line: usize) -> usize {
    let ls = doc.line_starts();
    let start = ls.get(line).unwrap_or(doc.len());
    match ls.get(line + 1) {
        Some(next) => (next.saturating_sub(1)).min(doc.len()).max(start),
        None => doc.len(),
    }
}

/// 第 `line` 行的源码文本（O(行长)；`line_str` 同源）。
pub fn line_text(doc: &Doc, line: usize) -> String {
    doc.line_str(line)
}
