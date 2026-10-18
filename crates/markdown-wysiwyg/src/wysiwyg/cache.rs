use gpui::{Pixels, SharedString};
use crate::markdown::wysiwyg::element;
use crate::markdown::wysiwyg::{CodeParseKey, CodeParseState, MathSlot};

/// 基础布局代：`(struct_ver, mermaid_zoom, shaping 宽)` 折成一个 u64——**结构性内容**/宽度/
/// mermaid 缩放 任一变 → 整篇重建；非结构性编辑不改 `struct_ver` → 布局跨键复用，只做
/// Fenwick 点更新。mermaid 缩放（`zgen`）也进代：缩放只影响单张卡片高，但块高存在基础
/// 布局数组里，必须整篇重建一次才一致（缩放是低频操作，可接受；不复用稀疏 delta 路线）。
/// **不含 `mgen` 与图片代（`images_gen`）**：数学排版就绪只改**卡片高**（走 `fold_plan`
/// 稀疏 delta）、图片占位→真实/拖拽改宽只改**含图块高**（走 `patch_pending_image_heights`
/// 稀疏定向覆写）——基础布局里数学/图片块的 base 是普通段/占位估算高，与就绪/改宽无关
/// ⟹ 把 `mgen`/`images_gen` 放这里会让每个公式光栅完成/每 tick 图片拖拽都触发全 139k 块
/// Pass1 重建（M3 A4 前 `images_gen` 曾在代里：拖拽每 tick 2-5ms/10 万块近全量估算）。
/// 纯算术混淆（不做 SipHash：15.9 万块逐块键构造会吃掉 ~200ms）。
pub(crate) fn height_epoch(struct_ver: u64, width_bits: u32, zgen: u64) -> u64 {
    let a = struct_ver ^ (width_bits as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let b = (width_bits as u64).wrapping_mul(0x517C_C1B7_2722_0A95)
        ^ zgen.wrapping_mul(0x243F_6A88_85A3_08D3);
    let x = a ^ b.rotate_left(32);
    x ^ x >> 33
}

/// 与 element prepaint 同式的可视带量程（q=512）：缩放锚定偏移写回后据此推导新带，
/// 保证 `prepare_font_zoom` 重建的帧与 measure/prepaint 一致（避免跨界二次重建与
/// `shape_band` 瞬态不一致）。`top` = 元素局部 y 的视口顶部（内容坐标）。
pub(crate) fn zoom_band_for_top(top: f32, viewport_h: Pixels) -> (f32, f32) {
    let q = 512.;
    let vh = viewport_h.as_f32().max(1.);
    (
        (((top - vh) / q).floor() * q).max(0.),
        ((top + 2. * vh) / q).ceil() * q,
    )
}

// ═══ 泛型 LRU 核心 ═══
// 三份手抄 LRU（RowShape / MathSlot / CodeParse）的「(clock,key) BTreeSet + 满容量淘汰 +
// 插入摘旧钟 + CacheWrite{old,evicted}」收敛于此。语义差异（查询是否 touch——MathSlot
// 是 `get` 不 touch / 外部 `touch`；淘汰是否返回——RowShape 需释放内部资源；键是否
// "携带代"）由各缓存**薄封装**保留（见 [`RowShapeCache`]/[`MathSlotCache`]/
// [`CodeParseCache`]），泛型核心只承担三份共有的「BTreeSet LRU + 满容量淘汰 + 插入
// 摘旧钟 + CacheWrite」。逐字段与三份旧实现对齐，行为零变化。

/// LRU 组合键：需 `Ord`（BTreeSet 升序）、`Clone`（移插）、`Hash + Eq`（HashMap 键）。
/// `u64`（RowShape 内容指纹）/`SharedString`（MathSlot）/`CodeParseKey` 均满足。
pub(crate) trait LruKey: Clone + Ord + std::hash::Hash + Eq {}
impl LruKey for u64 {}
impl LruKey for SharedString {}
impl LruKey for CodeParseKey {}

/// 一个缓存条目：值 + 最近使用钟 + 供 BTreeSet 组合键的 key。
///
/// `key` 对两类缓存分别为：
/// - RowShape（内容寻址 + 换代）：条目键 = 缓存键（内容指纹），换代的代键随值
///   `RowShapeEntry.key` 携带——重插同指纹时旧条目被摘钟、新条目复用同一 key ⇒
///   `(clock, key)` 组合键恒唯一（clock 更换即同 key 也严格有序）。
/// - MathSlot / CodeParse（简单键）：条目键 = 缓存键本身（`identity`）。
pub(crate) struct LruEntry<K: LruKey, V> {
    last_seen: u32,
    key: K,
    value: V,
}

/// 泛型 LRU 缓存（时钟序 BTreeSet 淘汰：对数找最久未用、对数插——替代每次满缓存插入
/// 全表扫 `min(last_seen)` 的 O(cap)）。读/写语义差异由各缓存薄封装决定：
/// - [`LruCache::get`]：不改顺序的纯查询（MathSlot paint 热路径、CodeParse `contains`）；
/// - [`LruCache::get_with_touch`]：命中即换新钟（MathSlot 外部 `touch`、CodeParse
///   `lookup`、RowShape 命中读）；
/// - [`LruCache::put`]：满容量且键不在场 → 淘汰 BTreeSet 队首（最久未用）；已存在同名
///   条目（换代重插）先摘旧钟。返回覆盖旧槽 + 被淘汰槽（[`CacheWrite`]）。
pub(crate) struct LruCache<K: LruKey, V> {
    /// 缓存键 → 条目（`RowShapeCache.entries` 测试断言读它）。
    pub(crate) entries: std::collections::HashMap<K, LruEntry<K, V>>,
    /// `(last_seen, key)` 升序 LRU 索引：首元素即最久未用条目；组合键用 `key` 解
    /// `last_seen` 相同（u32 clock 回绕后）的歧义，淘汰顺序确定。
    lru: std::collections::BTreeSet<(u32, K)>,
    cap: usize,
    clock: u32,
}

impl<K: LruKey, V: Clone> LruCache<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            lru: std::collections::BTreeSet::new(),
            cap,
            clock: 0,
        }
    }

    /// 纯查询（O(1)，不移动 LRU 次序；paint/形状热路径）。
    fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    /// 命中：更新访问钟并返回条目。内部换钟 = 移除旧 `(last_seen, key)` 再插新钟
    /// （`clock` 单调进、组合键唯一 → 绝不残留过期键）。
    fn get_with_touch(&mut self, key: &K) -> Option<&mut V> {
        let e = self.entries.get_mut(key)?;
        let last = e.last_seen;
        e.last_seen = self.clock;
        self.lru.remove(&(last, e.key.clone()));
        self.lru.insert((self.clock, e.key.clone()));
        self.clock = self.clock.wrapping_add(1);
        Some(&mut e.value)
    }

    /// 标记「最近使用」（键不在场则 no-op）。`clock` 换钟同 [`Self::get_with_touch`]。
    fn touch(&mut self, key: &K) {
        let Some(e) = self.entries.get_mut(key) else {
            return;
        };
        let last = e.last_seen;
        e.last_seen = self.clock;
        self.lru.remove(&(last, e.key.clone()));
        self.lru.insert((self.clock, e.key.clone()));
        self.clock = self.clock.wrapping_add(1);
    }

    /// 插入/替换：满容量且键不在场 → LRU 淘汰 BTreeSet 队首（最久未用条目，`evicted`
    /// 交调用方释放，如 `MathSlot::Ready` 位图纹理）；已存在同名条目（换代重插）先摘
    /// 旧钟再重插（`old` = 被覆盖旧值）。`key_of` 给条目写 key（RowShape 的 content 键
    /// 派生；MathSlot/CodeParse 恒 `identity`）。
    fn put(
        &mut self,
        key: K,
        value: V,
        key_of: impl Fn(&K, &V) -> K,
    ) -> CacheWrite<V> {
        let mut evicted = None;
        if self.entries.len() >= self.cap
            && !self.entries.contains_key(&key)
            && let Some(&(_, ref k)) = self.lru.iter().next()
        {
            let k = k.clone();
            let last = self.entries[&k].last_seen;
            self.lru.remove(&(last, k.clone()));
            evicted = self.entries.remove(&k).map(|e| e.value);
        }
        let old = if let Some(e) = self.entries.get(&key) {
            let last = e.last_seen;
            self.lru.remove(&(last, key.clone()));
            Some(e.value.clone())
        } else {
            None
        };
        let entry_key = key_of(&key, &value);
        self.entries.insert(
            key.clone(),
            LruEntry {
                last_seen: self.clock,
                key: entry_key,
                value,
            },
        );
        self.lru.insert((self.clock, key));
        self.clock = self.clock.wrapping_add(1);
        CacheWrite { old, evicted }
    }

    /// 按键移除（返回被移除的值；测试/定向作废用）。
    fn remove(&mut self, key: &K) -> Option<V> {
        let e = self.entries.remove(key)?;
        self.lru.remove(&(e.last_seen, e.key));
        Some(e.value)
    }

    /// 全部取走并清空（换源 / 主题色变化整批清除；调用方负责释放 `Ready` 纹理）。
    fn drain_all(&mut self) -> Vec<V> {
        self.lru.clear();
        self.entries.drain().map(|(_, e)| e.value).collect()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    #[cfg(test)]
    fn keys(&self) -> std::collections::hash_map::Keys<'_, K, LruEntry<K, V>> {
        self.entries.keys()
    }
}

/// 一次缓存写入的结果：覆盖的旧槽 + 被 LRU 淘汰的槽。淘汰槽（`MathSlot::Ready` 的
/// 位图纹理）由调用方负责释放（`cx.drop_image`）——本仓唯一带 GPU 纹理的缓存值。
pub(crate) struct CacheWrite<T> {
    pub(crate) old: Option<T>,
    pub(crate) evicted: Option<T>,
}

/// 一整行（块）的**整形内容缓存**（内容寻址，键 = 块内容指纹 `block_content_id`）。
/// 滚动/带重建时复用已整形块，不再把可视带整批重 shape（`shape_line` ）并为每个代码块
/// 重跑 syntect——结构密集代码文档「滚动每档 65ms」的主因（对齐 `view` 的行虚拟化复用
/// + `editor` 「只重排变更区」的原则）。
///
/// - 键 = 内容指纹（kind/跨度/首末行文本）^ 换代键 `key`（宽度/mermaid 缩放）：
///   **不随块下标/行号位移**——结构编辑（Enter/合并）后位移但内容未变的块指纹不变 →
///   命中旧行，免整带重排（旧实现按块下标键，结构编辑让后缀块下标全位移 → 整带 miss
///   重排 ~6ms debug）；内容真正变化的块指纹变 → 自然 miss。
/// - 内容变化靠指纹覆盖；数学就绪/图片尺寸给特定块换高 → `invalidate_ids` 定向作废；
///   换源/主题字号变化 → 整体 `clear`。
/// - 结构性编辑（块数/块序变）/ 换源 / 主题字号变化 → 整体 `clear`。
/// - caret 严格落在**含行内公式的文本行**时会 reveal（shape 依赖光标）→ 该行不缓存
///   （调用方 `caret_dep` 判定）。
/// - 容量上限见 [`config::limits::SHAPED_ROW_CACHE_CAP`]，超出按最近最少使用淘汰。
///   淘汰用 `lru` 索引（`(last_seen, id)` 组合键升序 `BTreeSet`）取首元素 = 最久未用条目，
///   O(log cap)/插——替代旧实现每次满缓存插入都全表扫 `min(last_seen)` 的 O(cap)（滚动超
///   大文档 ≈14k 次插 × 16k 表项比较）。
pub(crate) struct RowShapeCache {
    /// 内容指纹 → 条目（`frame::block_content_id` 键；`shape_rows_has` 测试断言读它）。
    pub(crate) entries: LruCache<u64, RowShapeEntry>,
}

#[derive(Clone)]
pub(crate) struct RowShapeEntry {
    key: u64,
    row: std::rc::Rc<element::ShapedRow>,
}

fn identity_key<K: Clone, V>(k: &K, _v: &V) -> K {
    k.clone()
}

impl RowShapeCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: LruCache::new(cap),
        }
    }
    /// 命中：更新访问钟并返回 `Rc`（浅克隆即可取得行，heavy 数据全是 Arc/Rc）。
    /// 键换代（`RowShapeEntry.key != 查询 key`）→ 不命中、也不移动 LRU 次序。
    pub(crate) fn get(&mut self, id: u64, key: u64) -> Option<std::rc::Rc<element::ShapedRow>> {
        // 键在场且一致才更新访问钟（换代不 touch）；先只读判定再取行克隆。
        if !self.key_mismatch_unchecked(id, key) {
            return None;
        }
        let row = self.entries.entries.get_mut(&id).unwrap().value.row.clone();
        self.entries.touch(&id);
        Some(row)
    }
    /// 键在场且一致（换代/key 一致才更新访问钟）。
    fn key_mismatch_unchecked(&self, id: u64, key: u64) -> bool {
        self.entries
            .entries
            .get(&id)
            .is_some_and(|e| e.value.key == key)
    }
    /// 该块存在**但键已换代**（宽度/mermaid 缩放变化把旧行逐出）——与"从未缓存"区分：
    /// 换代重排时旧 `settled/verified/attempts` 判决对应的是上一代结果，应作废重来。
    pub(crate) fn key_mismatch(&self, id: u64, key: u64) -> bool {
        self.entries
            .entries
            .get(&id)
            .is_some_and(|e| e.value.key != key)
    }
    /// 插入（新行）；容量满时淘汰 LRU 索引首元素（最久未用条目）。返回缓存的 `Rc`。
    /// 换代重插同指纹：旧条目被摘钟后新条目复用同一 cache 键（`(clock, id)` 组合键恒
    /// 唯一，见 [`LruEntry`] 注释）。本缓存不向调用方返回淘汰/覆盖的旧行（旧实现同；
    /// 行内 heavy 数据是 Arc/Rc，局部丢弃即释放引用）。
    pub(crate) fn put(
        &mut self,
        id: u64,
        key: u64,
        row: element::ShapedRow,
    ) -> std::rc::Rc<element::ShapedRow> {
        let rc = std::rc::Rc::new(row);
        let _ = self.entries.put(
            id,
            RowShapeEntry {
                key,
                row: rc.clone(),
            },
            identity_key,
        );
        rc
    }
    /// 逐个作废（数学就绪/内容变化的受影响块；键 = 内容指纹）。
    pub(crate) fn invalidate_ids(&mut self, ids: &[u64]) {
        for &b in ids {
            self.entries.remove(&b);
        }
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

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
        self.entries.get(&key.into())
    }
    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(&key.into())
    }
    /// 标记「最近使用」：`sync_math` 每帧对带内公式统一调用（带内 = LRU 命中面）。
    pub(crate) fn touch(&mut self, key: &str) {
        self.entries.touch(&key.into());
    }
    /// 插入并标记最近使用；容量满且键不在场 → LRU 淘汰最久未用条目（`evicted` 由
    /// 调用方释放 `Ready` 纹理）。返回覆盖旧值 + 淘汰槽。
    pub(crate) fn insert(&mut self, key: SharedString, slot: MathSlot) -> CacheWrite<MathSlot> {
        self.entries.put(key, slot, identity_key)
    }
    /// 按键移除（返回被移除的槽；测试/定向作废用）。
    #[cfg(test)]
    pub(crate) fn remove(&mut self, key: &str) -> Option<MathSlot> {
        self.entries.remove(&key.into())
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
/// 含 `.notdef`（回退未命中）块的**真实平台重排次数**上限。gpui 的 `LineLayoutCache`
/// 把平台 `layout_line` 结果按 `(text, font, runs)` 键缓存、与 wrap-width 无关——扰动
/// 宽度永远打不到 DWrite（115c131 的失败根因）。因此自愈改走「冷却一帧」：每次真实
/// 重排后让该键一帧不被引用 → 两帧缓存自然淘汰 → 下一次重排才是新键、真打到平台。
/// 连续 `NOTDEF_RETRY_ATTEMPTS` 次真实平台重排仍坏 → 判定字符确实无字形（正确显示
/// tofu），落入 `settled` 从此不再重排空转。冷启动 DWrite 首次 shape 回退字形可能给
/// .notdef、第二次同 (font,size) 即正常——**自愈约 3 帧就够**；8 次（≈0.27s@30fps）
/// 比 3 帧仍富余、又比旧值 15 次（≈0.5s）减半冗余（M5-B4 瘦身）；重试仅在真 notdef
/// 时才占预算，普通文本零成本。
pub(crate) const NOTDEF_RETRY_ATTEMPTS: u32 = 8;

// ═══ M4 跳过归档：本仓 P1 冷启动 emoji 缺陷的**永久驻留防御**（勿删）═══
// P1 冷启动 emoji：Windows COLR 光栅失败被字模缓存钉住，上游 zed（含 93d7456）未修；
// M4 vendor gpui_windows+[patch] 已决定跳过 → 本组件为本仓永久驻留防御，勿删；若升级
// gpui 或引入 vendor 修复可整体移除并删除本注释。
// 本类型（+ `NOTDEF_RETRY_ATTEMPTS`）与 `warm.rs`、Pass2 的 notdef/回退校验桥接同属
// 这套防御体系：`verified`（非零占位字形校验）、`last`（冷却帧几何复用防闪跳）、
// `cool_start`/`verify_cool`（冷却一帧破 LineLayoutCache 自愈）共为一块，改任何一处
// 前先读 `WY_NOTDEF_DEBUG` 实测取证（`docs/debug-envs.md`），勿各自裁剪。

/// `.notdef`（回退未命中）坏块的自愈状态机（被 [`element::build_frame`] Pass2 驱动）。
/// 每次「真实平台重排」出 notdef 后登记一次并转冷却；冷却帧**不调用 gpui shape**——
/// 让 `LineLayoutCache` 里该 (text,font,runs) 键自然过期（两帧缓存只在当前/上一帧存
/// 活），下一次重排才是全新键、真的重新打到 DWrite。回退解析成功（或任何冷却行恢复）
/// 即整体标记「回退已温」，此后 notdef = 确无字形、直接入缓存不再空转。
#[derive(Default)]
pub(crate) struct NotdefRetry {
    /// 块 → 已做的真实平台重排次数（达到 [`NOTDEF_RETRY_ATTEMPTS`] 上限 → `settled`）。
    pub attempts: rustc_hash::FxHashMap<usize, u32>,
    /// 处于冷却帧的块（下一帧不 shape，复用 `last` 几何）。
    pub cooling: rustc_hash::FxHashSet<usize>,
    /// 已判定「确无字形」的块：命中缓存时不再当 miss 重排（稳定，避免空转）。
    pub settled: rustc_hash::FxHashSet<usize>,
    /// 冷却帧用于继续显示的上一份 notdef 行（几何与新鲜重排一致，不闪跳）。
    pub last: rustc_hash::FxHashMap<usize, std::rc::Rc<element::ShapedRow>>,
    /// 已做过「回退字形校验重排」的块：冷启动首次 shape 用到回退字体（❗⚠✅ 等）时，
    /// DWrite 可能给非零但错误的占位字形（非 .notdef，glyph id≠0 检测不到）→ 冷却一次
    /// 强制第二次真实重排并缓存那次结果；标记后直接走正常缓存。
    pub verified: rustc_hash::FxHashSet<usize>,
}

impl NotdefRetry {
    /// 单块状态整体作废（内容编辑/前缀保留外块）。
    pub(crate) fn clear_block(&mut self, block: usize) {
        self.attempts.remove(&block);
        self.cooling.remove(&block);
        self.settled.remove(&block);
        self.verified.remove(&block);
        self.last.remove(&block);
    }

    /// 干净排成功后的收尾：清 notdef 重试循环（attempts/cooling/settled/last），
    /// **保留**回退校验标记 `verified`。同一（text,font,runs）的回退字形解析与折行宽无关，
    /// 一次校验即长期有效（`verified` 的原始语义就是「已做第二次真实重排、此后直接走正常
    /// 缓存」）。若连 verified 一起清掉，任何一次缓存 miss 重建（滚动/点击清帧/宽度换代）
    /// 都会重新走 `fallback-verify`「首排不缓存→冷却→次排」——而首排回退字形 advance 不稳定
    /// （真机实测书尾注释章 [!90] 行 776 vs 782，差 6px）→ 同一行相邻帧按两种 wrap 显示
    /// （句号跨行翻转）。`clear_stale`（换代）/`retain_prefix`（编辑）才是 verified 的正确
    /// 回收点。
    pub(crate) fn clear_notdef_keep_verified(&mut self, block: usize) {
        self.attempts.remove(&block);
        self.cooling.remove(&block);
        self.settled.remove(&block);
        self.last.remove(&block);
    }

    /// 「换代」作废：块被**新代**键重新整形前，清掉上一代的判决（attempts/settled/
    /// verified/last），**保留**冷却位——冷却帧仍复用上一份几何，换代重排不闪跳。
    /// 作用：图片解码完成触发 `igen` 换代重建时，若某块曾在 DWrite 退化期（解码卡顿
    /// 窗口）被误判 `settled`（预算烧光）或 `verified`（非零占位字形），换代后重新
    /// 校验/重试即可自愈，不再整次运行永久豆腐块。
    pub(crate) fn clear_stale(&mut self, block: usize) {
        self.attempts.remove(&block);
        self.settled.remove(&block);
        self.verified.remove(&block);
        self.last.remove(&block);
    }

    /// 前缀保留：结构编辑后下标 < `keep` 的块未位移，重试/冷却/settled 状态保留。
    pub(crate) fn retain_prefix(&mut self, keep: usize) {
        if keep == 0 {
            self.clear();
            return;
        }
        self.attempts.retain(|&k, _| k < keep);
        self.cooling.retain(|&k| k < keep);
        self.settled.retain(|&k| k < keep);
        self.verified.retain(|&k| k < keep);
        self.last.retain(|&k, _| k < keep);
    }

    pub(crate) fn clear(&mut self) {
        self.attempts.clear();
        self.cooling.clear();
        self.settled.clear();
        self.verified.clear();
        self.last.clear();
    }
}

/// 冷启动回退自愈诊断：`WY_NOTDEF_DEBUG=1` 时把首启探针/重试/给出的过程打到 stderr，
/// 用于在真实 Windows 首启复现 `.notdef` 豆腐块问题时拿到确切证据（不开启则零开销）。
#[inline]
pub(crate) fn notdef_debug(args: std::fmt::Arguments<'_>) {
    if std::env::var_os("WY_NOTDEF_DEBUG").is_some() {
        eprintln!("[WY-NOTDEF] {args}");
    }
}
