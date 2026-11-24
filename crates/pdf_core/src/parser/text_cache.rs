//! worker 侧页文本缓存（[`PageText`] 按文档+页键控的有界 LRU）。
//!
//! 问题：`WorkerMsg::Search` 每次查询都把 `next_page` 归零重扫全文，`search_slice` 对每页调
//! [`super::extract_page_text`]（FPDF_LoadPage + FPDFText_LoadPage + 每字符 3 个 FFI），防抖后
//! 改一个字符 → 全文重提取一遍（87 页文本 PDF 实测 ~440ms）。解法：把页文本放进**工作线程持久**
//! 状态（非 `PendingSearch` 临时态），同文档多查询复用；Text 预取路径同池共用（双向暖心）。
//!
//! 失效语义：`DocId` 进程级单调不复用、文档重开必换新 ID、`Close` 整档清空
//! （[`PageTextCache::clear_doc`]）→ `(DocId, page)` 即天然代次正确，无需额外文本代次号。
//!
//! 有界：字节预算 [`SEARCH_TEXT_CACHE_BYTES`]，超预算按最久未用淘汰（命中提升到队尾）；
//! 仅 worker 单线程内访问（与 `docs` 同生命周期），无需内部锁。

use std::collections::{HashMap, VecDeque};

use super::{DocId, PageText, PdfTextChar};

/// 页文本缓存总预算（字节）。每页 ~20-100KB（config 注释口径），64MB 可容 ~600-1000 页全文——
/// 典型文档一次搜索即整段驻留、后续查询零重提取；超大文档按最近访问淘汰、内存有界。
pub(crate) const SEARCH_TEXT_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// worker 侧按 `(doc_id, page)` 键控的页文本缓存（有界 LRU）。
pub(crate) struct PageTextCache {
    map: HashMap<(DocId, usize), PageText>,
    /// 访问序（队尾 = 最近使用；预算淘汰取队首，命中提升到队尾）。
    order: VecDeque<(DocId, usize)>,
    /// 当前占用估算字节（[`PageText`] 头 + 字符数组容量 × 单字符尺寸）。
    bytes: u64,
    /// 字节预算（生产 = [`SEARCH_TEXT_CACHE_BYTES`]；测试可压小钉死淘汰行为）。
    budget: u64,
}

impl PageTextCache {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget: SEARCH_TEXT_CACHE_BYTES,
        }
    }

    /// 测试覆盖（仅 `cfg(test)`）：把预算压到极小钉死「超预算逐出最久未用」的预期行为。
    #[cfg(test)]
    pub(crate) fn set_budget_for_test(&mut self, budget: u64) {
        self.budget = budget;
    }

    /// 命中返回页文本引用并提升 LRU 序；未中返回 `None`（调用方经
    /// [`super::cached_page_text`] 抽取并回填）。
    pub(crate) fn get(&mut self, doc_id: DocId, page: usize) -> Option<&PageText> {
        let key = (doc_id, page);
        if self.map.contains_key(&key) {
            self.touch(key);
        }
        self.map.get(&key)
    }

    /// 回填页文本（缓存未中后的抽取结果；同键覆盖——内容应一致）；超预算逐出最久未用。
    pub(crate) fn insert(&mut self, doc_id: DocId, page: usize, text: PageText) {
        let key = (doc_id, page);
        let cost = Self::entry_bytes(&text);
        match self.map.insert(key, text) {
            Some(old) => {
                self.bytes = self.bytes.saturating_sub(Self::entry_bytes(&old));
                self.touch(key);
            }
            None => self.order.push_back(key),
        }
        self.bytes = self.bytes.saturating_add(cost);
        self.evict_to_budget();
    }

    /// 文档关闭即整档清空（释放内存；DocId 单调不复用，防残留键随时间无界增长）。
    pub(crate) fn clear_doc(&mut self, doc_id: DocId) {
        self.order.retain(|(d, _)| *d != doc_id);
        let keys: Vec<(DocId, usize)> = self
            .map
            .keys()
            .filter(|(d, _)| *d == doc_id)
            .copied()
            .collect();
        for k in keys {
            if let Some(t) = self.map.remove(&k) {
                self.bytes = self.bytes.saturating_sub(Self::entry_bytes(&t));
            }
        }
    }

    /// 把键提升到队尾（命中即最近使用）。
    fn touch(&mut self, key: (DocId, usize)) {
        if let Some(idx) = self.order.iter().position(|k| *k == key) {
            self.order.remove(idx);
        }
        self.order.push_back(key);
    }

    /// 预算收敛：超预算时逐出最久未用（队首 = 最早写入/最久未命中）。
    fn evict_to_budget(&mut self) {
        while self.bytes > self.budget && !self.order.is_empty() {
            let key = self.order.pop_front().expect("order 非空才有候选");
            if let Some(t) = self.map.remove(&key) {
                self.bytes = self.bytes.saturating_sub(Self::entry_bytes(&t));
            }
        }
    }

    /// 页文本占用估算（[`PageText`] 头 + 字符数组容量 × 单字符时序尺寸）。
    fn entry_bytes(t: &PageText) -> u64 {
        std::mem::size_of::<PageText>() as u64
            + t.chars.capacity() as u64 * std::mem::size_of::<PdfTextChar>() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(i: usize, n_chars: usize) -> PageText {
        PageText {
            page_index: i,
            width: 100.0,
            height: 100.0,
            chars: vec![
                PdfTextChar {
                    ch: 'a',
                    left: 0.0,
                    bottom: 0.0,
                    right: 1.0,
                    top: 1.0,
                    synthetic: false,
                    origin_x: 0.0,
                    origin_y: 0.0,
                };
                n_chars
            ],
            synthetic_count: 0,
        }
    }

    /// 有界性：超字节预算逐出**最久未用**（插入序队首）。
    #[test]
    fn lru_evicts_oldest_by_byte_budget() {
        let mut c = PageTextCache::new();
        c.set_budget_for_test(4000); // 约 1 页（100 字符 ≈ 3.2KB）
        c.insert(0, 0, page(0, 100));
        c.insert(0, 1, page(1, 100));
        assert!(c.get(0, 0).is_none(), "超预算应淘汰最久未用 (0,0)");
        assert!(c.get(0, 1).is_some(), "最近插入 (0,1) 应保留");
        assert!(c.bytes <= c.budget, "占用字节不应超预算: {}", c.bytes);
    }

    /// 命中提升：被访问过的键成为「最近使用」，后续逐出的是次旧者。
    #[test]
    fn get_promotes_recent_use() {
        let mut c = PageTextCache::new();
        c.set_budget_for_test(7000); // 恰容 2 页（各 ~3.2KB）
        c.insert(0, 0, page(0, 100));
        c.insert(0, 1, page(1, 100));
        assert!(c.get(0, 0).is_some(), "命中 (0,0) 提升为最近使用");
        c.insert(0, 2, page(2, 100)); // 超预算 → 逐出最久未用 = (0,1)
        assert!(c.get(0, 1).is_none(), "命中过 (0,0) 后应淘汰 (0,1)");
        assert!(c.get(0, 0).is_some(), "(0,0) 应保留（刚被提升）");
        assert!(c.get(0, 2).is_some(), "最新插入 (0,2) 应保留");
        assert!(c.bytes <= c.budget, "占用字节不应超预算: {}", c.bytes);
    }

    /// 关闭整档清空（防残留键随开闭增长；内存释放）。
    #[test]
    fn clear_doc_removes_all_pages_of_doc() {
        let mut c = PageTextCache::new();
        c.set_budget_for_test(1_000_000);
        c.insert(7, 0, page(0, 10));
        c.insert(7, 1, page(1, 10));
        c.insert(8, 0, page(0, 10));
        c.clear_doc(7);
        assert!(
            c.get(7, 0).is_none() && c.get(7, 1).is_none(),
            "close 后该 doc 页应全清"
        );
        assert!(c.get(8, 0).is_some(), "其它文档不受影响");
    }
}
