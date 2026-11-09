//! 宿主侧缓存（`MathSlotCache`/`CodeParseCache`）。
//!
//! 引擎渲染面（`RowShapeCache`/`NotdefRetry`/`NOTDEF_RETRY_ATTEMPTS`/LRU 泛型核心/
//! `height_epoch`/`zoom_band_for_top`/`notdef_debug`）已迁入 `markdown_wysiwyg_engine::cache`；
//! 本模块是原 `wysiwyg/cache.rs` 拆出后**留宿主**的部分：`MathSlotCache`（依赖
//! `markdown_wysiwyg_math::MathSlot`）与 `CodeParseCache`（依赖宿主 `CodeParseKey`/
//! `CodeParseState`），经 `wysiwyg::cache_host` 由 `wysiwyg/mod.rs` 重导出（
//! `wysiwyg::{MathSlotCache, CodeParseCache}` 原路径不变），复用引擎的
//! `LruCache`/`CacheWrite`/`identity_key` 泛型核心，行为与拆分前逐字一致。
//! 原「`LruKey` 标记 trait」已随迁入引擎后移除（`CodeParseKey` 是外部 tuple 别名，无法
//! 为外部 trait 补 impl），`LruCache` 直接以 `Clone + Ord + Hash + Eq` 为键界。

use gpui::SharedString;
use markdown_wysiwyg_engine::cache::{CacheWrite, LruCache, identity_key};
use super::{CodeParseKey, CodeParseState, MathSlot};
#[cfg(test)]
use markdown_wysiwyg_engine::cache::LruEntry;

/// 数学排版槽缓存：把无界 `HashMap<SharedString, MathSlot>` 收成「容量上限 + LRU」
/// （内存只涨不降修复：同一标签页持续打开不同公式文档时位图/GPU 纹理只涨不降）。
///
/// 取舍（注释写明）：
/// - **读路径 `get` 保持 O(1) 纯查询、不移动 LRU 次序**——paint/形状期是热路径，
///   每帧每公式都拆插 BTreeSet 会白烧。「最近使用」由 `sync_math`（每帧遍历窗口带内
///   公式）在循环顶的 `touch` 维护：带内公式（含 Loading/Ready/Failed）= 最近使用，
///   滚出带外久未回来的公式停止被 touch → 依次进入淘汰面——对「窗口化懒光栅 + 滚动」
///   的真实形态命中率足够，不精确于每一笔 paint 读。
/// - 写路径 `insert` 满容量且键不在场时按 `(clock, key)` 升序 BTreeSet 淘汰最久未用
///   条目，O(log cap)，与 [`RowShapeCache`] 同款；cap=1000 见
///   [`config::limits::MATH_SLOTS_CACHE_CAP`] 取舍。
pub(crate) struct MathSlotCache {
    entries: LruCache<SharedString, MathSlot>,
}

impl MathSlotCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: LruCache::new(cap),
        }
    }
    /// 纯查询（O(1)，不移动 LRU 次序；paint/形状热路径）。
    pub(crate) fn get(&self, key: &str) -> Option<&MathSlot> {
        self.entries.get(key)
    }
    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    /// 标记「最近使用」：`sync_math` 每帧对带内公式统一调用（带内 = LRU 命中面）。
    pub(crate) fn touch(&mut self, key: &str) {
        self.entries.touch(key);
    }
    /// 插入并标记最近使用；容量满且键不在场 → LRU 淘汰最久未用条目（`evicted` 由
    /// 调用方释放 `Ready` 纹理）。返回覆盖旧值 + 淘汰槽。
    pub(crate) fn insert(&mut self, key: SharedString, slot: MathSlot) -> CacheWrite<MathSlot> {
        self.entries.put(key, slot, identity_key)
    }
    /// 按键移除（返回被移除的槽；测试/定向作废用）。
    #[cfg(test)]
    pub(crate) fn remove(&mut self, key: &str) -> Option<MathSlot> {
        self.entries.remove(key)
    }
    /// 全部取走并清空（换源 / 主题色变化整批清除；调用方负责释放 `Ready` 纹理）。
    pub(crate) fn drain_all(&mut self) -> Vec<MathSlot> {
        self.entries.drain_all()
    }
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub(crate) fn keys(
        &self,
    ) -> std::collections::hash_map::Keys<'_, SharedString, LruEntry<SharedString, MathSlot>> {
        self.entries.keys()
    }
}

/// 代码块高亮解析缓存：把无界 `HashMap<CodeParseKey, CodeParseState>` 收成
/// 「容量上限 + LRU」。读路径 `lookup` 触摸最近使用（shape 期每块 O(log cap)，
/// cap=512 ⇒ ≤9 级；解析 runs 是纯 CPU 数据，淘汰无纹理可释放）。
pub(crate) struct CodeParseCache {
    entries: LruCache<CodeParseKey, CodeParseState>,
}

impl CodeParseCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: LruCache::new(cap),
        }
    }
    /// 查询 + 标记最近使用（O(log cap)；shape 期对带内代码块调用）。
    pub(crate) fn lookup(&mut self, key: &CodeParseKey) -> Option<&CodeParseState> {
        self.entries.get_with_touch(key).map(|v| &*v)
    }
    pub(crate) fn contains_key(&self, key: &CodeParseKey) -> bool {
        self.entries.contains_key(key)
    }
    /// 插入并标记最近使用；容量满且键不在场 → LRU 淘汰最久未用条目（CPU 数据直接丢）。
    pub(crate) fn insert(
        &mut self,
        key: CodeParseKey,
        state: CodeParseState,
    ) -> CacheWrite<CodeParseState> {
        self.entries.put(key, state, identity_key)
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}
