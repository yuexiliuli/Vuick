//! 跨帧滚动预热状态（引擎侧 `Prewarm`/`PrewarmSig`）。
//!
//! 自宿主 `markdown_wysiwyg::wysiwyg/prewarm.rs` 迁入（纯结构移动）：依赖仅
//! [`crate::element::Frame`] / [`crate::element::ShapedRow`]（引擎纯类型）；宿主经
//! `wysiwyg::prewarm` re-export 别名保持原路径（`Prearm`/`PrewarmSig`）。驱动函数
//! `prewarm_band`/`settle_giant_blocks` 仍留宿主（依赖 `WyEditor`/`shape_docrow`）。
use crate::element::Frame;

/// 跨帧滚动预热进度（见 [`element::prewarm_band`]）：方向只由带序号跨界
/// （`band.top/512` 取整变化）决定，量子内小滚不换向；预算按块数硬上限；
/// `bias` = 目标带内滑动窗起点，每帧推进、回绕兜底（保证总有进度、永不卡死）。
///
/// 静置门控（Sprint 4b）：预整形只在「滚动活跃期」工作——`sig` 记录上一帧的
/// （滚动像素/宽度/内容代）签名，连续无变化帧累计到 [`Self::IDLE_SKIP_FRAMES`] 后停止，
/// 滚动/缩放/编辑等任何变化立即复位。登记簿（`last_band_idx`/`dir`/`bias`）在跳过帧
/// 仍由 [`element::prewarm_band`] 更新，滚动恢复后速度预算/方向判定保持新鲜。
///
/// I4（巨型代码块整形跳过）：`giant_pin`/`pending_giant`/`giant_idle`/`giant_sig` 是
/// 「巨块快速路径 → 静止权威整形」的跨帧状态，见 [`super::frame::settle_giant_blocks`]。
#[derive(Clone)]
pub struct Prewarm {
    pub dir: i8,
    /// 基础预算（每帧预热块数的下限；慢滚/静止时用它维持对下一带的渐进填充）。
    pub blocks_per_frame: usize,
    pub bias: usize,
    pub last_band_idx: Option<i32>,
    /// 连续「滚动/内容/宽度全未变」的帧计数；任何变化复位为 0。
    pub idle: u8,
    /// 上一 prewarm 帧的变化签名；任一维与当前帧不同都视为「有滚动/缩放/内容意图」。
    pub sig: Option<PrewarmSig>,
    /// 冷开强制预整形的**剩余帧数**（Step4-③）：`install_open` 换源后置
    /// [`Self::COLD_TOPUP_FRAMES`]，这些帧内 `idle_skip` **无视静置门控** 放行
    /// `prewarm_band`（每帧 8ms 预算内），把首视口周边带提前 shape 进缓存（否则静置
    /// 门控会在滚动前就把预热门关掉 → 用户首次滚动落点带冷、Pass2 突发回归）。
    /// 任何滚动/内容/宽度变化（签名变）立即清零 → 回归正常门控（0746946 不破坏）。
    pub cold_topup: u8,
    /// I4 巨块**钉住行**（键 = 巨块专属粗指纹 [`super::frame::giant_block_id`]，非全内容
    /// [`super::frame::block_content_id`]）：最后一次权威整形的巨型代码块行。编辑作废
    /// `shape_rows` 后仍钉住复用——巨块内键入期间文本/光标/点击继续可用（内容滞后至上次
    /// settle，静止后由 `settle_giant_blocks` 更新为新内容）。粗指纹首/次末行在「中段
    /// 键入」中稳定 → 旧钉继续可见（全内容指纹会让每击换键 → 巨块闪回占位板）。按粗指纹
    /// 键 → 结构编辑/换源后旧条目自然失效（不触碰 edit.rs）。
    pub giant_pin: std::collections::HashMap<u64, std::rc::Rc<crate::element::ShapedRow>>,
    /// 巨块钉**插入序**（FIFO，先 pin 先逐出）：`giant_pin` 超出 [`Self::GIANT_PIN_CAP`]
    /// 时 `pop_front` 最旧 pid 并从 map `remove`（HashMap 无序，逐出次序由本队列
    /// 保证）。只在新 pid 首次 pin 时入队，与 map 同步增删（见 [`Self::pin_giant`]）。
    pub pin_order: std::collections::VecDeque<u64>,
    /// 巨块全内容键 memo（性能，`giant_fast_row` 每帧消费）：`block_index → (content_gen,
    /// key)`——内容代（[`crate::frame::content_gen`]）未变直接复用，滚动/光标/宽窄变化
    /// 不改代 → 巨块每帧免 O(块) 全内容重 hash（`block_content_id` 逐 chunk 折叠大块）；
    /// 内容编辑才换代重算（跨帧引用状态）；换源重建后内容代必自增 → 条目自然判旧
    /// 重算，无需显式清空（真值以 content_gen 判定，见 [`Self::memo_giant_content_key`]）。
    pub giant_key_memo:
        std::collections::HashMap<usize, (u64, u64)>,
    /// I4 待「静止权威整形」的巨型代码块下标（去重；`settle_giant_blocks` 消费）。
    pub pending_giant: Vec<usize>,
    /// I4 静止权威整形驱动：连续「签名无变化」帧计数（语义同 [`Self::idle`]，独立于
    /// 预热门控，编辑/滚动/宽度任一变复位——保证每键不整块、静止后才补）。
    pub giant_idle: u8,
    /// I4 静止整形**上一帧**签名（同 [`PrewarmSig`] 语义；与预热门控签名各记各的）。
    pub giant_sig: Option<PrewarmSig>,
}

/// 帧变化签名（见 [`Prewarm::sig`]）——「内容/宽度维」与 `build_frame` 帧缓存键**同源**
/// （`width/rgen/igen/mgen/zgen`，见 [`element::Frame`]）：任一维变化都意味着
/// 视口/缩放/内容动过了，本帧应恢复预整形。`scroll_top` = 调用方传入的**未量程化**
/// 滚动像素（prepaint 的 `top`）——prepaint 的 `frame.band` 是 512px 量子化栅格，
/// 带内几像素的微滚不动带（`frame.band` 逐帧相同），签名若只看 `band` 会把「仍在
/// 慢速滚动、还没跨带」误判为静置、把预热门关掉 → 跨带时落点带冷、Pass2 突发回归
/// （本 sprint 要消除的粘滞）。未量程化的 `scroll_top` 逐帧追滚动，杜绝该误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrewarmSig {
    /// 未量程化滚动像素（f32 位型）。滚动任意微动即变 → 门控放行。
    pub scroll_top: u32,
    pub width_bits: u32,
    pub rgen: u64,
    pub igen: u64,
    pub mgen: u64,
    pub zgen: u64,
}

impl PrewarmSig {
    pub fn of(frame: &Frame, scroll_top: f32) -> Self {
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
            cold_topup: 0,
            giant_pin: std::collections::HashMap::new(),
            pin_order: std::collections::VecDeque::new(),
            giant_key_memo: std::collections::HashMap::new(),
            pending_giant: Vec::new(),
            giant_idle: 0,
            giant_sig: None,
        }
    }
}

impl Prewarm {
    /// 冷开强制预整形帧数（Step4-③）：首帧 `prewarm_band` 只登记带序号（无整形）——
    /// 帧 1 消耗 1 帧、帧 2.. 每帧 8ms 预算预整形下一带；≈6 帧（0.2s@30fps）足够首
    /// 视口下几带温好。比时长更关键的是预算：预整形单帧仍有 **8ms 硬时间上限**
    /// （`prewarm_band` 内 `time_budget`），冷开帧不会叠加成长帧。
    pub const COLD_TOPUP_FRAMES: u8 = 6;

    /// I4 静止权威整形等待帧数：巨块快速路径登记后，连续 `GIANT_SETTLE_IDLE_FRAMES`
    /// 帧签名无变化（静止/停编辑）才补一次权威整形——太短会插进打字连击中间造成
    /// 单帧秒级冻结；8 帧 ≈ 0.27s@30fps，足够识别真正的停顿。
    pub const GIANT_SETTLE_IDLE_FRAMES: u8 = 8;

    /// I4 巨块钉上界：`giant_pin` 最多保留 [`Self::GIANT_PIN_CAP`] 个巨块钉（防长会话
    /// 巨块钉只涨不降、孤儿条目永不回收）；超限逐出**最先 pin** 的 pid（先 pin 先逐出，
    /// 次序由 [`Self::pin_order`] FIFO 保证）。
    pub const GIANT_PIN_CAP: usize = 16;

    /// I4 登记「待静止权威整形」的巨块（去重；`settle_giant_blocks` 消费）。
    pub fn register_giant(&mut self, block: usize) {
        if !self.pending_giant.contains(&block) {
            self.pending_giant.push(block);
        }
    }
    /// I4 取指定内容指纹的钉住行（快速路径复用；None = 无钉/已换代）。
    pub fn giant_stale(&self, cid: u64) -> Option<std::rc::Rc<crate::element::ShapedRow>> {
        self.giant_pin.get(&cid).cloned()
    }
    /// I4 更新钉住行（`settle_giant_blocks` 权威整形落盘）。超上限逐出**最先 pin** 的
    /// pid（先 pin 先逐出，`giant_pin.len()` 恒 ≤ [`Self::GIANT_PIN_CAP`]）。
    pub fn pin_giant(&mut self, cid: u64, row: std::rc::Rc<crate::element::ShapedRow>) {
        let is_new = !self.giant_pin.contains_key(&cid);
        self.giant_pin.insert(cid, row);
        if is_new {
            self.pin_order.push_back(cid);
        }
        while self.giant_pin.len() > Self::GIANT_PIN_CAP {
            let Some(oldest) = self.pin_order.pop_front() else {
                break;
            };
            self.giant_pin.remove(&oldest);
        }
    }
    /// 巨块全内容键 memo（性能，消费方 `giant_fast_row`）：`content_gen` 与记忆一致直接
    /// 复用；否则现算 [`crate::frame::block_content_id`] 并记忆。内容代 = 帧级内容版本
    /// （[`crate::frame::content_gen`]），滚动/光标/宽窄变化不改代 → 巨块每帧免 O(块)
    /// 全内容重 hash；内容编辑换代才重算。换源重建内容代自增 → 条目自然判旧，无需清空。
    pub fn memo_giant_content_key(
        &mut self,
        content_gen: u64,
        doc: &markdown::doc::Doc,
        k: usize,
    ) -> u64 {
        if let Some(&(g, key)) = self.giant_key_memo.get(&k) {
            if g == content_gen {
                return key;
            }
        }
        let key = crate::frame::block_content_id(doc, k);
        self.giant_key_memo.insert(k, (content_gen, key));
        key
    }

    /// 装源后冷开反门控：置 `cold_topup` 并让方向/登记簿全新（首帧重登记当前带），
    /// 之后每帧在预算内强制预整形，直到帧数耗尽或用户任何举动（签名变）打断（打断在
    /// [`Self::idle_skip`] 内联——见其文档）。
    pub fn arm_cold_topup(&mut self, frames: u8) {
        self.cold_topup = frames.max(1);
        // 全新方向：始终向「下一个带」预整形（首视口下方——用户首次滚动最可能的方向；
        // 上方由滚动恢复时 `dir` 反转补，兜底正确性不受影响）。
        self.dir = 1;
        self.last_band_idx = None;
        self.bias = 0;
        self.idle = 0;
        self.sig = None;
    }
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
    pub const MAX_BUDGET: usize = 24;
    pub fn effective_budget(&self, vel: usize) -> usize {
        (self.blocks_per_frame.max(1) + vel.saturating_mul(4)).min(Self::MAX_BUDGET)
    }

    /// **静置跳过阈值**：连续若干帧「滚动/内容/宽度」全无变化后停止预整形。
    /// 取 2 = 变化帧跑一次 + 再给 1 帧沉降余量；自第 2 个无变化帧起跳过
    /// （≈ 滚动停稳 ~1 帧后即停。滚动恢复下一帧立即复原，见 Sprint 4b 需求）。
    pub const IDLE_SKIP_FRAMES: u8 = 2;

    /// 本帧是否应**跳过**预整形循环（静置门）。签名与上次相同 → `idle += 1`，达到
    /// [`Self::IDLE_SKIP_FRAMES`] 返回 `true`；任何变化 → 复位门控并放行。
    /// `scroll_top` = 调用方从 prepaint 传入的**未量程化**滚动像素（`frame.band` 是
    /// 512px 量子栅格，带内微滚看不出差异 → 必须用原始值，见 [`PrewarmSig`]）。
    /// 调用方（`prewarm_band`）务必**先**更新登记簿（`last_band_idx`/`dir`/`bias`）
    /// 再调本方法，保证跳过帧的滚动状态仍新鲜。
    /// 冷开反门控（Step4-③）：`cold_topup > 0` 时**无视静置门控**持续放行（预算内
    /// 预整形首视口周边带）；签名任何变化（滚动/编辑/宽度）→ 立即清零冷开期回归
    /// 正常门控。
    pub fn idle_skip(&mut self, frame: &Frame, scroll_top: f32) -> bool {
        let sig = PrewarmSig::of(frame, scroll_top);
        if self.sig == Some(sig) {
            self.idle = self.idle.saturating_add(1);
        } else {
            // 装源后首帧登记（None→Some）不算用户变化（那是冷开期本身）；之后任何
            // 滚动/缩放/内容/宽度变化 → 冷开期结束、回归正常门控。
            if self.sig.is_some() {
                self.cold_topup = 0;
            }
            self.idle = 0;
            self.sig = Some(sig);
        }
        if self.cold_topup > 0 {
            self.cold_topup -= 1;
            return false;
        }
        self.idle >= Self::IDLE_SKIP_FRAMES
    }
}
