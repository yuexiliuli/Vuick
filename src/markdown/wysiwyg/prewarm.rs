/// 跨帧滚动预热进度（见 [`element::prewarm_band`]）：方向只由带序号跨界
/// （`band.top/512` 取整变化）决定，量子内小滚不换向；预算按块数硬上限；
/// `bias` = 目标带内滑动窗起点，每帧推进、回绕兜底（保证总有进度、永不卡死）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prewarm {
    pub dir: i8,
    /// 基础预算（每帧预热块数的下限；慢滚/静止时用它维持对下一带的渐进填充）。
    pub blocks_per_frame: usize,
    pub bias: usize,
    pub last_band_idx: Option<i32>,
}

impl Default for Prewarm {
    fn default() -> Self {
        Self {
            dir: 0,
            blocks_per_frame: 3,
            bias: 0,
            last_band_idx: None,
        }
    }
}

impl Prewarm {
    /// **速度自适应预算**：`vel` = 本帧带序号的跳跃量（= 滚动速度，带/帧）。
    ///
    /// 快滚时视口每帧跨多个 512px 量子带，固定 3 块/帧的预热追不上 → 落点带仍冷、Pass2
    /// 每带穿越补一次 ~6.4ms 整形突发（方波帧耗时 = “粘滞”来源）。按速度放大预算：
    /// - `vel=0`（静止/带内微滚）→ 只用基础预算，维持对下一带的渐进填充不空转；
    /// - `vel=1-2`（匀速滚）→ 预算 ~2 帧盖满一档（~20 块），连续带穿越变缓存命中；
    /// - `vel≥4`（拖条/猛滑）→ 预算接近一档块数，单帧即把下一落点带填满。
    ///
    /// 硬上限 [`Self::MAX_BUDGET`]（≈一档短段落块数）兜底最坏帧内整形量；预算单位是
    /// “本帧新 shape 的块数”，与 `prewarm_band` 的 `done` 计数同式。
    pub(crate) const MAX_BUDGET: usize = 24;
    pub(crate) fn effective_budget(&self, vel: usize) -> usize {
        (self.blocks_per_frame.max(1) + vel.saturating_mul(4)).min(Self::MAX_BUDGET)
    }
}
