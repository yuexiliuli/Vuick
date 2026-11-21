//! worker 侧「未着色原图」raw 缓存条目与预算簇（[`RawEntry`] + `cache_raw` + 预算查询）。
//!
//! raw 缓存语义：**单真值 = 未着色 bgra**；切档（`SetRecolor`）只对缓存内原图重跑
//! `apply_recolor` 现算新着色副本回传刷新可见区，不失效缓存槽、不重调 pdfium 栅格。
//! 预算 = [`config::pdf::RAW_CACHE_BYTES`]（高倍可见集的 3×封面 + 瓦片镜像），超支先入先出
//! 回收最旧；测试可经 [`set_raw_cache_budget_for_test`] 压到极小钉死 FIFO 回收预期行为。
//!
//! 跨簇可见性（模块重组）：`RawEntry` 由 `render`（finalize_render 构造/读取）与根 `run`
//! （4.5 重着色刷新按 key 查缓存）直接读写字段 → 结构+字段升 `pub(crate)`；`cache_raw` 由
//! `render::finalize_render` 调用 → `pub(crate)`。

use std::sync::atomic::{AtomicU64, Ordering};

use super::RawKind;

/// worker 侧「未着色原图」缓存条目（单真值 = raw bgra；切档重着色由 raw 现算现发）。
/// 与 view 层 [瓦片 key=(row,col) / 封面单槽 / 中清 / 缩略] 同构键控，另存密度标尺
/// `scale` 与几何，供重着色刷新回传后 view 侧按原几何替换显示位图。
pub(crate) struct RawEntry {
    pub(crate) kind: RawKind,
    pub(crate) page: usize,
    pub(crate) row: u32,
    pub(crate) col: u32,
    pub(crate) x0: u32,
    pub(crate) y0: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) full_w: u32,
    pub(crate) full_h: u32,
    pub(crate) scale: f32,
    pub(crate) bgra: Vec<u8>,
}

impl RawEntry {
    /// 跨可见性（模块重组）：`parser/tests.rs` 引用（缓存预算账目求和）→ 升 `pub(crate)`。
    pub(crate) fn bytes(&self) -> u64 {
        self.bgra.len() as u64
    }
}

/// 测试覆盖（仅 `#[cfg(test)]`）：raw 缓存预算的进程级覆盖——`set_raw_cache_budget_for_test`
/// 可把预算压到极小（如 2MB）以钉死「48MB 预算紧张 → 首可视页封面被 FIFO 挤出 → 切档部分
/// 刷新」的预期行为（WARN-2 专项测试）。0 = 用生产常量 [`config::pdf::RAW_CACHE_BYTES`]。
/// 须像门控/曲线一样持 pdfium 全局串行锁时设置、测试结束还原（同 K28 并发纪律）。
/// 跨 crate 可见性：静态恒编译（仅 `raw_cache_budget` 的 `#[cfg(test)]` 分支读它，生产惰性），
/// 三个钩子取消 `#[cfg(test)]` 供主 crate `pdf` 测试模块直接经重导出调用（非测试构建下惰性）。
static RAW_CACHE_BUDGET_OVERRIDE: AtomicU64 = AtomicU64::new(0);
#[doc(hidden)]
pub fn set_raw_cache_budget_for_test(v: u64) {
    RAW_CACHE_BUDGET_OVERRIDE.store(v, Ordering::Relaxed);
}
#[doc(hidden)]
pub fn raw_cache_budget_override() -> u64 {
    RAW_CACHE_BUDGET_OVERRIDE.load(Ordering::Relaxed)
}
/// 当前 raw 缓存预算：测试覆盖非 0 → 用它（极小值钉死 FIFO 回收）；默认生产常量。
#[doc(hidden)]
pub fn raw_cache_budget() -> u64 {
    #[cfg(test)]
    {
        let o = RAW_CACHE_BUDGET_OVERRIDE.load(Ordering::Relaxed);
        if o != 0 {
            return o;
        }
    }
    config::pdf::RAW_CACHE_BYTES
}

/// 把本轮栅格化的**未着色原图**写入文档 raw 缓存（预算超支时先入先出回收最旧）。
/// 同 key（kind/页/格号）同密度同尺寸 → 复用既有原图、不重复占用，返回 `false`（调用方
/// 仍可用本轮刚栅格化的同内容 raw 作着色出口）；其余情况写入/替换并返回 `true`。
/// `budget` = 该缓存的字节预算（生产传 [`config::pdf::RAW_CACHE_BYTES`]；测试可传极小值
/// 验证 FIFO 回收，K27 证据入仓）。
///
/// 跨簇可见性（模块重组）：`render::finalize_render` 调用 → 升 `pub(crate)`。
pub(crate) fn cache_raw(cache: &mut Vec<RawEntry>, entry: RawEntry, budget: u64) -> bool {
    let key_same = |e: &RawEntry| {
        e.kind == entry.kind
            && e.page == entry.page
            && e.row == entry.row
            && e.col == entry.col
    };
    if let Some(e) = cache.iter_mut().find(|e| key_same(e)) {
        if (e.scale - entry.scale).abs() <= 1e-4 && e.w == entry.w && e.h == entry.h {
            return false; // 同密度同尺寸：内容必然一致，复用既有 raw（不重复占内存）
        }
        *e = entry; // 密度/尺寸已变 → 旧条目过期，替换
    } else {
        cache.push(entry);
    }
    // 预算收敛：超支时回收最旧（队首 = 最早写入；单文档缓存规模有界，O(n) 扫足够）。
    let mut over: i64 = cache.iter().map(RawEntry::bytes).sum::<u64>() as i64 - budget as i64;
    while over > 0 && !cache.is_empty() {
        over -= cache.remove(0).bytes() as i64;
    }
    true
}
