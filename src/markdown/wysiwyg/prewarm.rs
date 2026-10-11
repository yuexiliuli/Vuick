use super::Frame;

/// 跨帧滚动预热进度（见 [`element::prewarm_band`]）：方向只由带序号跨界
/// （`band.top/512` 取整变化）决定，量子内小滚不换向；预算按块数硬上限；
/// `bias` = 目标带内滑动窗起点，每帧推进、回绕兜底（保证总有进度、永不卡死）。
///
/// 静置门控（Sprint 4b）：预整形只在「滚动活跃期」工作——`sig` 记录上一帧的
/// （滚动像素/宽度/内容代）签名，连续无变化帧累计到 [`Self::IDLE_SKIP_FRAMES`] 后停止，
/// 滚动/缩放/编辑等任何变化立即复位。登记簿（`last_band_idx`/`dir`/`bias`）在跳过帧
/// 仍由 [`element::prewarm_band`] 更新，滚动恢复后速度预算/方向判定保持新鲜。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prewarm {
    pub dir: i8,
    /// 基础预算（每帧预热块数的下限；慢滚/静止时用它维持对下一带的渐进填充）。
    pub blocks_per_frame: usize,
    pub bias: usize,
    pub last_band_idx: Option<i32>,
    /// 连续「滚动/内容/宽度全未变」的帧计数；任何变化复位为 0。
    pub idle: u8,
    /// 上一 prewarm 帧的变化签名；任一维与当前帧不同都视为「有滚动/缩放/内容意图」。
    pub sig: Option<PrewarmSig>,
}

/// 帧变化签名（见 [`Prewarm::sig`]）——「内容/宽度维」与 `build_frame` 帧缓存键**同源**
/// （`width/rgen/igen/mgen/zgen`，见 [`element::Frame`]）：任一维变化都意味着
/// 视口/缩放/内容动过了，本帧应恢复预整形。`scroll_top` = 调用方传入的**未量程化**
/// 滚动像素（prepaint 的 `top`）——prepaint 的 `frame.band` 是 512px 量子化栅格，
/// 带内几像素的微滚不动带（`frame.band` 逐帧相同），签名若只看 `band` 会把「仍在
/// 慢速滚动、还没跨带」误判为静置、把预热门关掉 → 跨带时落点带冷、Pass2 突发回归
/// （本 sprint 要消除的粘滞）。未量程化的 `scroll_top` 逐帧追滚动，杜绝该误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PrewarmSig {
    /// 未量程化滚动像素（f32 位型）。滚动任意微动即变 → 门控放行。
    pub scroll_top: u32,
    pub width_bits: u32,
    pub rgen: u64,
    pub igen: u64,
    pub mgen: u64,
    pub zgen: u64,
}

impl PrewarmSig {
    pub(crate) fn of(frame: &Frame, scroll_top: f32) -> Self {
        Self {
            scroll_top: scroll_top.to_bits(),
            width_bits: f32::from(frame.width).to_bits(),
            rgen: frame.rgen,
            igen: frame.igen,
            mgen: frame.mgen,
            zgen: frame.zgen,
        }
    }
}

impl Default for Prewarm {
    fn default() -> Self {
        Self {
            dir: 0,
            blocks_per_frame: 3,
            bias: 0,
            last_band_idx: None,
            idle: 0,
            sig: None,
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

    /// **静置跳过阈值**：连续若干帧「滚动/内容/宽度」全无变化后停止预整形。
    /// 取 2 = 变化帧跑一次 + 再给 1 帧沉降余量；自第 2 个无变化帧起跳过
    /// （≈ 滚动停稳 ~1 帧后即停。滚动恢复下一帧立即复原，见 Sprint 4b 需求）。
    pub(crate) const IDLE_SKIP_FRAMES: u8 = 2;

    /// 本帧是否应**跳过**预整形循环（静置门）。签名与上次相同 → `idle += 1`，达到
    /// [`Self::IDLE_SKIP_FRAMES`] 返回 `true`；任何变化 → 复位门控并放行。
    /// `scroll_top` = 调用方从 prepaint 传入的**未量程化**滚动像素（`frame.band` 是
    /// 512px 量子栅格，带内微滚看不出差异 → 必须用原始值，见 [`PrewarmSig`]）。
    /// 调用方（`prewarm_band`）务必**先**更新登记簿（`last_band_idx`/`dir`/`bias`）
    /// 再调本方法，保证跳过帧的滚动状态仍新鲜。
    pub(crate) fn idle_skip(&mut self, frame: &Frame, scroll_top: f32) -> bool {
        let sig = PrewarmSig::of(frame, scroll_top);
        if self.sig == Some(sig) {
            self.idle = self.idle.saturating_add(1);
        } else {
            self.idle = 0;
            self.sig = Some(sig);
        }
        self.idle >= Self::IDLE_SKIP_FRAMES
    }
}
