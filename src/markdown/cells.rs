//! GFM 表格「单元格边界」的共享字节机制（预览 `parser` 与 WYSIWYG `live` 共用）。
//!
//! 两套管线对「`|` 单元格边界」的判定一致（`markdown::parity::tables_cells_agree` 已钉死），
//! 但对**单元格内容**有文档化差异（`known_differences_are_documented` #7）：
//! - 预览（`parser::split_table_cells`）把 `\|`→`|`、`\\`→`\` 解义（显示）；
//! - live（`table_cells`）保留转义原样（源码逐字节，光标/IME 映射用）。
//!
//! 本模块只共享**边界判定**（最易错的部分：反斜杠奇偶转义），内容清理由各自消费方决定，
//! 避免同一字节机制写两遍后未来漂移。

/// 把 `s` 在**未转义**的 `|` 处切开（转义竖线 `\|` 是字面内容，不是分隔符，按 GFM）。
/// 转义符会留在返回切片里（单元格是逐字节源码），`table_cells`/光标映射需要精确的源码映射，
/// 重写时按格再合会原样往返。
pub(crate) fn split_unescaped_pipes(s: &str) -> impl Iterator<Item = &str> {
    let b = s.as_bytes();
    let mut cuts = vec![0usize];
    for (i, &c) in b.iter().enumerate() {
        if c == b'|' && !is_backslash_escaped(b, i) {
            cuts.push(i + 1);
        }
    }
    cuts.push(s.len() + 1);
    (0..cuts.len() - 1).map(move |k| &s[cuts[k]..cuts[k + 1] - 1])
}

/// 字节 `i` 是否有转义前缀：前面有**奇数**个 `\`。
pub(crate) fn is_backslash_escaped(b: &[u8], i: usize) -> bool {
    let mut n = 0;
    while n < i && b[i - 1 - n] == b'\\' {
        n += 1;
    }
    n % 2 == 1
}

/// 列对齐（GFM 分隔行 `:---` / `:---:` / `---:` 语义）——**唯一的对齐实现**，
/// 预览 `parser`、WYSIWYG `live`、语义 IR 三方共用（各自 enum 只做薄转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellAlign {
    Left,
    Center,
    Right,
}

/// 从分隔行单元格解析每列对齐；任一格不合规返回 `None`。
pub(crate) fn parse_aligns(cells: &[&str]) -> Option<Vec<CellAlign>> {
    cells
        .iter()
        .map(|c| {
            let body = c.trim_start_matches(':').trim_end_matches(':');
            if body.is_empty() || !body.chars().all(|ch| ch == '-') {
                return None;
            }
            Some(match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => CellAlign::Center,
                (false, true) => CellAlign::Right,
                _ => CellAlign::Left,
            })
        })
        .collect()
}

/// 表格行的原始单元格（trim + 去掉首尾外竖线压出的空 cell，**保留转义原样**）——
/// 与 live `table_cells` 同语义；预览显示层在其上叠加解义。用作 IR 的表格结构。
pub(crate) fn raw_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    split_unescaped_pipes(t)
        .map(|s| s.trim().to_string())
        .collect()
}
