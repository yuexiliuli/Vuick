//! 文档级检索簇：命中/结果型（[`PdfSearchHit`] / [`PdfSearchResults`]）+ 单页匹配
//! （[`search_page_text`]）+ in-flight 分片续跑状态（[`PendingSearch`] / [`search_slice`]）。
//!
//! 调度：`WorkerMsg::Search` 只登记 [`PendingSearch`] 状态，根 `run` 在无渲染/占位积压时
//! 每轮调 [`search_slice`] 扫一片（几页/几 ms）——检索期间的渲染永不整段冻结。
//!
//! 跨簇可见性（模块重组）：`PendingSearch` / `search_slice` 被根 `run` 消费 → 结构+字段
//! 与函数升 `pub(crate)`；`extract_page_text`（来自 `extract` 簇）被分片检索调用。

use std::collections::HashMap;
use std::sync::mpsc;

use pdfium_render::prelude::*;

use super::{DocId, PageText, extract_page_text};

/// 一次文档级检索的命中：页号 + 页内**原始字符区间**（`[start, end)`，与
/// [`PageText::chars`](PageText) 下标一致）+ 就地生成的**上下文片段**
/// （前/后各 [`SEARCH_SNIPPET_CTX`] 字，侧边栏逐行展示用；不依赖阅读器文本缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfSearchHit {
    pub page: usize,
    pub start: usize,
    pub end: usize,
    /// `前5字 + 命中目标 + 后5字`（左右不足一侧带 `…` 省略标记）。
    pub snippet: String,
}

/// 文档级检索结果（**自包含**、可跨线程传给阅读器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfSearchResults {
    /// 全部命中（页号升序，页内按文本顺序；非重叠）。
    pub hits: Vec<PdfSearchHit>,
    /// 命中数是否因超过上限被截断。
    pub truncated: bool,
}

/// 文档检索命中数上限（防超大文档/单字节查询返回海量结果撑爆 UI）。
pub(crate) const SEARCH_MAX_HITS: usize = 1000;
/// 命中上下文片段每侧字数（`前5字 + 目标 + 后5字`）。
pub(crate) const SEARCH_SNIPPET_CTX: usize = 5;

/// 单页文本层内做（ASCII 大小写不敏感、非重叠）子串匹配，返回**原始字符区间**。
///
/// 基于已抽取的 `chars` 重建"可见码点序列"（跳过 NUL——pdfium 生成的填充），
/// 用 `to_ascii_lowercase` 折叠字母（CJK/全角不受影响，仍按精确字匹配）。
/// 返回 `(start, end)`：`start` 为匹配首字符下标，`end` 为匹配末字符下标 +1。
pub(crate) fn search_page_text(text: &PageText, needle: &[char]) -> Vec<(usize, usize)> {
    let m = needle.len();
    if m == 0 || text.chars.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text
        .chars
        .iter()
        .enumerate()
        .filter(|(_, c)| c.ch != '\0')
        .map(|(i, c)| (i, c.ch))
        .collect();
    if hay.len() < m {
        return Vec::new();
    }
    let fold = |c: char| c.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + m <= hay.len() {
        let mut matched = true;
        for k in 0..m {
            if fold(hay[i + k].1) != fold(needle[k]) {
                matched = false;
                break;
            }
        }
        if matched {
            out.push((hay[i].0, hay[i + m - 1].0 + 1));
            i += m; // 非重叠：跳过已匹配段
        } else {
            i += 1;
        }
    }
    out
}

/// 全文档检索分片参数：worker 侧每次续跑最多扫 [`SEARCH_SLICE_PAGES`] 页、且耗时不超过
/// [`SEARCH_SLICE_TIMEOUT`]，就回主循环 → 让排队的 Render/RenderTile 插队执行。没有它，
/// 一次全文档同步扫描会在检索期间冻结所有瓦片/封面（87 页文本 PDF 实测检索 ~440ms，
/// 期间到达的渲染被堵到检索扫完全文——正是“随机页先模糊、数秒后清晰”的根因）。
const SEARCH_SLICE_PAGES: usize = 4;
const SEARCH_SLICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(6);

/// worker 内的一次在途全文档检索（分片续跑）：由 `WorkerMsg::Search` 建立，主循环在
/// 无渲染/占位积压时每轮到 [`search_slice`] 扫一片；文档中途被 Close → 直接取已扫到的
/// 部分结果收尾，绝不长期独占工作线程。
///
/// 跨簇可见性（模块重组）：根 `run` 直接读写字段 / 调 [`PendingSearch::is_done`] →
/// 结构+字段与 `is_done` 升 `pub(crate)`。
pub(crate) struct PendingSearch {
    pub(crate) doc_id: DocId,
    pub(crate) needle: Vec<char>,
    pub(crate) next_page: usize,
    pub(crate) page_count: usize,
    pub(crate) hits: Vec<PdfSearchHit>,
    pub(crate) truncated: bool,
    pub(crate) tx: mpsc::Sender<Option<PdfSearchResults>>,
}

impl PendingSearch {
    /// 是否已扫完（下一页越界）或命中数已到上限（截断）。
    pub(crate) fn is_done(&self) -> bool {
        self.truncated || self.next_page >= self.page_count
    }
}

/// 单页检索：把该页命中合并进 `hits`；已达上限置 `truncated` 并停止追加。页无效/无文本 → 无命中。
fn search_one_page_into(
    doc: &PdfDocument<'_>,
    page_ix: usize,
    needle: &[char],
    hits: &mut Vec<PdfSearchHit>,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }
    let Some(text) = extract_page_text(doc, page_ix) else {
        return;
    };
    for (s, e) in search_page_text(&text, needle) {
        let snippet = crate::pdf::layout::search_snippet(&text, s, e, SEARCH_SNIPPET_CTX);
        hits.push(PdfSearchHit {
            page: page_ix,
            start: s,
            end: e,
            snippet,
        });
        if hits.len() >= SEARCH_MAX_HITS {
            *truncated = true;
            return;
        }
    }
}

/// 主循环调用的“一片检索”：最多扫 [`SEARCH_SLICE_PAGES`] 页、耗时不超过
/// [`SEARCH_SLICE_TIMEOUT`]，随后返回让其它消息处理。文档被 Close → 置为结束。
///
/// 跨簇可见性（模块重组）：根 `run` 调用 → 升 `pub(crate)`。
pub(crate) fn search_slice(docs: &mut HashMap<DocId, PdfDocument<'_>>, ps: &mut PendingSearch) {
    let start = std::time::Instant::now();
    for _ in 0..SEARCH_SLICE_PAGES {
        if ps.is_done() || start.elapsed() > SEARCH_SLICE_TIMEOUT {
            return;
        }
        match docs.get(&ps.doc_id) {
            Some(doc) => {
                search_one_page_into(doc, ps.next_page, &ps.needle, &mut ps.hits, &mut ps.truncated);
                ps.next_page += 1;
            }
            None => {
                // 文档已被 Close：收尾（返回已扫到的部分结果）
                ps.page_count = ps.next_page;
                return;
            }
        }
    }
}
