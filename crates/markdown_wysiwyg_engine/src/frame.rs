//! WYSIWYG **帧装配层（frame）**（自宿主 `wysiwyg/frame/mod.rs` 迁入引擎）：把已 shape 的行
//! 按可视带组装成 [`crate::element::Frame`]（带外块按 [`crate::shape::estimate_height`] 占位）；
//! 含 notdef 自愈的「回退字体行」识别与诊断、**D1 滚动预热**（`prewarm_band`）与巨型代码块
//! 「静止权威整形」。同时收纳 `build_frame` 依赖、原宿主持有的**渲染模型编排函数**（`derived`/
//! `fold_plan`/`fold_reveal`/`ensure_block_layout`/图片缓存失效与稀疏覆写/notdef 桥接/数学宽与
//! 钉缓存）——它们只读写 `rd` + `doc`（引擎不依赖宿主），宿主相应方法收缩为转发胶水。
//!
//! 迁移 seam：入参统一 `rd: &RefCell<WyRenderModel>`（显式参数 + 内部可变）＋
//! `host: &dyn WyRenderHost`（宿主只读态）；`rd` 与 `host` 字段互不同时可变借用。

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{Pixels, Window, px};

use crate::cache::{height_epoch, notdef_debug};
use crate::element::*;
use crate::model::{ImageSrcBlocks, PrebuildBlock, PrebuildPass1, WyDerived, WyRenderModel};
use crate::prewarm::PrewarmSig;
use crate::shape::{build_math_card_row, estimate_height, image_block_base_height};
use crate::{WyRenderHost, docutil, theme::WyTheme};
use markdown::doc::{Doc, DocRow};
use markdown::ir::IrKind;
use markdown::parse::inline_style::{self, MathRegion};
use markdown::parse::{BlockMeta, RowKind};
use markdown_wysiwyg_mermaid::mermaid_card_height;

/// 块布局的可变引用（`ensure_block_layout` 返回 / 缓存槽共用；对齐宿主 types.rs 别名）。
pub(crate) type BlockLayoutRef = Rc<RefCell<BlockLayout>>;

/* ---------- rd 读取/写入小助手（由 btn 迁移的宿主对应方法收缩而来） ---------- */

/// 内容结构版本/代数（帧缓存键 + 基础布局代组成）。
pub fn content_gen(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().content_gen.get()
}
pub fn images_gen(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().images_gen.get()
}
pub fn math_gen(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().math_gen.get()
}
pub fn mermaid_zoom_gen(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().mermaid_zoom_gen.get()
}
pub fn struct_ver(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().struct_ver.get()
}
pub fn struct_dirty_line(rd: &RefCell<WyRenderModel>) -> usize {
    rd.borrow().struct_dirty_line.get()
}
pub fn old_edit_end_line(rd: &RefCell<WyRenderModel>) -> usize {
    rd.borrow().struct_old_end_line.get()
}
pub fn edit_line_delta(rd: &RefCell<WyRenderModel>) -> i64 {
    rd.borrow().struct_line_delta.get()
}
pub fn regions_ver(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().regions_ver.get()
}
pub fn regions_shift_ver(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().regions_shift_ver.get()
}
pub fn fold_ver(rd: &RefCell<WyRenderModel>) -> u64 {
    rd.borrow().fold_ver.get()
}
pub fn take_pending_patch_blocks(rd: &RefCell<WyRenderModel>) -> Option<Vec<usize>> {
    rd.borrow_mut().pending_patch_blocks.borrow_mut().take()
}
pub(crate) fn fold_plan_rc(rd: &RefCell<WyRenderModel>) -> Option<Rc<FoldPlan>> {
    rd.borrow().fold_plan.borrow().as_ref().map(|(_, _, p)| p.clone())
}
/// 某 mermaid 围栏块的缩放（缺省 1.0）。
pub fn mermaid_zoom_of(rd: &RefCell<WyRenderModel>, src_line: usize) -> f32 {
    rd.borrow()
        .mermaid_zoom
        .borrow()
        .get(&src_line)
        .copied()
        .unwrap_or(1.0)
}
/// 数学卡内容自然宽侧缓存写（px；键 = 数学块首源码行）。
pub fn set_math_natural_w(rd: &RefCell<WyRenderModel>, src_line: usize, w: f32) {
    if w > 0.01 {
        rd.borrow_mut().math_natural_w.borrow_mut().insert(src_line, w);
    } else {
        rd.borrow_mut().math_natural_w.borrow_mut().remove(&src_line);
    }
}
/// 录制/更新某块 `$$` 卡的最近就绪位图钉。
pub fn set_math_pin(
    rd: &RefCell<WyRenderModel>,
    src_line: usize,
    image: std::sync::Arc<gpui::RenderImage>,
    w: f32,
    h: f32,
) {
    rd.borrow_mut()
        .math_pins
        .borrow_mut()
        .insert(src_line, crate::model::BlockMathPin { image, w, h });
}

/* ---------- notdef 自愈桥接（rd 侧；原宿主 view.rs 方法逐字迁入） ---------- */

/// 冷却帧取上一份 notdef 行几何。
pub fn cooling_row(
    rd: &RefCell<WyRenderModel>,
    block: usize,
) -> Option<Rc<ShapedRow>> {
    let rdg = rd.borrow_mut();
    let mut m = rdg.notdef_retry.borrow_mut();
    if !m.cooling.remove(&block) {
        return None;
    }
    m.last.remove(&block)
}
/// 登记一次 notdef（推进真实平台重排计数 + 置冷却）。返回 `true` = 已达上限判定确无字形。
pub fn cool_start(rd: &RefCell<WyRenderModel>, block: usize) -> bool {
    let rdg = rd.borrow_mut();
    let mut m = rdg.notdef_retry.borrow_mut();
    let cnt = m.attempts.get(&block).copied().unwrap_or(0) + 1;
    if cnt >= crate::cache::NOTDEF_RETRY_ATTEMPTS {
        m.attempts.remove(&block);
        m.cooling.remove(&block);
        m.last.remove(&block);
        m.settled.insert(block);
        true
    } else {
        m.attempts.insert(block, cnt);
        m.cooling.insert(block);
        false
    }
}
/// notdef 后保存本帧几何（供随后的冷却帧显示复用）。
pub fn cool_keep_last(rd: &RefCell<WyRenderModel>, block: usize, row: Rc<ShapedRow>) {
    rd.borrow_mut().notdef_retry.borrow_mut().last.insert(block, row);
}
/// 该块是否已判定「确无字形」（settled）。
pub fn notdef_settled(rd: &RefCell<WyRenderModel>, block: usize) -> bool {
    rd.borrow().notdef_retry.borrow().settled.contains(&block)
}
/// 干净排成功后清 notdef 重试态（保留回退校验标记 verified）。
pub fn clear_notdef(rd: &RefCell<WyRenderModel>, block: usize) {
    rd.borrow_mut()
        .notdef_retry
        .borrow_mut()
        .clear_notdef_keep_verified(block);
}
/// 该块是否已做过「回退字形校验重排」。
pub fn fallback_verified(rd: &RefCell<WyRenderModel>, block: usize) -> bool {
    rd.borrow().notdef_retry.borrow().verified.contains(&block)
}
pub fn mark_fallback_verified(rd: &RefCell<WyRenderModel>, block: usize) {
    rd.borrow_mut().notdef_retry.borrow_mut().verified.insert(block);
}
/// 回退校验冷却：只置冷却标志（不占 notdef 重试次数）。
pub fn verify_cool(rd: &RefCell<WyRenderModel>, block: usize) {
    rd.borrow_mut().notdef_retry.borrow_mut().cooling.insert(block);
}

/// （仅宿主测试用）Pass2 光栅/冷却/预热计数器自增。宿主 `markdown_wysiwyg` 测试构建
/// 经 dev-dependency 叠加 `test-mode` feature（对齐 config 惯例）→ 正常编译下零开销。
#[cfg(feature = "test-mode")]
pub(crate) fn bump_reshapes(rd: &RefCell<WyRenderModel>) {
    let rdg = rd.borrow();
    rdg.reshapes.set(rdg.reshapes.get() + 1);
}
#[cfg(feature = "test-mode")]
pub(crate) fn bump_coolings(rd: &RefCell<WyRenderModel>) {
    let rdg = rd.borrow();
    rdg.coolings.set(rdg.coolings.get() + 1);
}
#[cfg(feature = "test-mode")]
pub(crate) fn bump_prewarm_shapes(rd: &RefCell<WyRenderModel>) {
    let rdg = rd.borrow();
    rdg.prewarm_shapes.set(rdg.prewarm_shapes.get() + 1);
}

/* ---------- 基础布局编排（原宿主 view.rs ensure_block_layout / derived / fold_plan） ---------- */

/// 校验基础布局命中当前（代, 块数）：命中返回 `(false, 新布局)` 供复用；未命中建出
/// 空布局并缓存，返回 `(true, 新)` 及第三个元素 = 旧布局（结构编辑增量 Pass1 用）。
pub fn ensure_block_layout(
    rd: &RefCell<WyRenderModel>,
    n_blocks: usize,
    epoch: u64,
) -> (bool, BlockLayoutRef, Option<BlockLayoutRef>) {
    if let Some((e, l)) = &*rd.borrow().block_layout.borrow()
        && *e == epoch
        && l.borrow().n() == n_blocks
    {
        return (false, l.clone(), None);
    }
    let old = rd.borrow().block_layout.borrow().as_ref().map(|(_, l)| l.clone());
    let l = Rc::new(std::cell::RefCell::new(BlockLayout::empty()));
    *rd.borrow_mut().block_layout.borrow_mut() = Some((epoch, l.clone()));
    (true, l, old)
}

/// 内容派生扫描缓存（全篇一遍，按 `regions_ver` 缓存）——`build_frame` / paint 用。
pub fn derived(rd: &RefCell<WyRenderModel>, doc: &Doc) -> Rc<RefCell<WyDerived>> {
    let ver = rd.borrow().regions_ver.get();
    if let Some((g, d)) = &*rd.borrow().derived.borrow()
        && *g == ver
    {
        return d.clone();
    }
    // Cut-1b：三个全篇扫描的**计数器预筛全空**（无 `$`、无 `[!`、无折叠标题）时，
    // derived 必为全空三表——直接返回，**不物化全文**。
    if rd.borrow().math_dollar_cnt.get() == 0
        && rd.borrow().alert_marker_cnt.get() == 0
        && rd.borrow().folded_headings.borrow().is_empty()
    {
        let d = Rc::new(RefCell::new(WyDerived {
            math_regions: Vec::new(),
            alert_fold_regions: Vec::new(),
            alert_region_kinds: Default::default(),
            heading_fold_bodies: Vec::new(),
        }));
        *rd.borrow_mut().derived.borrow_mut() = Some((ver, d.clone()));
        return d;
    }
    let content = doc.text_shared();
    let alerts = if rd.borrow().alert_marker_cnt.get() > 0 {
        inline_style::alert_regions(&content)
    } else {
        Vec::new()
    };
    let alert_fold_regions = alerts
        .iter()
        .filter_map(|(r, _, fold)| fold.map(|f| (r.clone(), f)))
        .collect();
    let mut alert_region_kinds: rustc_hash::FxHashMap<
        usize,
        markdown::parse::recognition::AlertKind,
    > = rustc_hash::FxHashMap::with_capacity_and_hasher(alerts.len() * 2, Default::default());
    for (r, k, _) in &alerts {
        for l in r.clone() {
            alert_region_kinds.insert(l, *k);
        }
    }
    let d = Rc::new(RefCell::new(WyDerived {
        math_regions: if rd.borrow().math_dollar_cnt.get() > 0 {
            inline_style::math_regions(&content)
        } else {
            Vec::new()
        },
        alert_fold_regions,
        alert_region_kinds,
        heading_fold_bodies: {
            let rdg = rd.borrow();
            let folded = rdg.folded_headings.borrow();
            if folded.is_empty() {
                Vec::new()
            } else {
                inline_style::heading_fold_regions(&content, &folded)
            }
        },
    }));
    *rd.borrow_mut().derived.borrow_mut() = Some((ver, d.clone()));
    d
}

/// 在**按 start 升序且互不重叠**的区域表上二分定位「包含 `line` 的区域」下标。
pub fn region_containing<T>(
    regions: &[T],
    range_of: impl Fn(&T) -> Range<usize>,
    line: usize,
    incl_start: bool,
) -> Option<usize> {
    let p = if incl_start {
        regions.partition_point(|r| range_of(r).start <= line)
    } else {
        regions.partition_point(|r| range_of(r).start < line)
    };
    let idx = p.checked_sub(1)?;
    (line < range_of(&regions[idx]).end).then_some(idx)
}

/// `fold_bodies`（= [alert 折叠体组 ‖ 标题折叠体组]）里「包含 caret 的折叠体」。
fn folded_body_at(
    fold_bodies: &[Range<usize>],
    fold_split: usize,
    line: usize,
) -> Option<&Range<usize>> {
    fold_bodies
        .get(..fold_split)
        .and_then(|g| region_containing(g, |r| r.clone(), line, false).map(|i| &g[i]))
        .or_else(|| {
            fold_bodies
                .get(fold_split..)
                .and_then(|g| region_containing(g, |r| r.clone(), line, false).map(|i| &g[i]))
        })
}

/// 由已增量维护的 `WyDerived` 构建 marker 集合。
fn build_fp_sets(derived: &Rc<RefCell<WyDerived>>) -> Rc<FoldPlanSets> {
    let d = derived.borrow();
    let math_regions = &d.math_regions;
    let _t_sets = crate::prof::begin();
    let mut math_start: rustc_hash::FxHashMap<usize, usize> =
        rustc_hash::FxHashMap::with_capacity_and_hasher(
            math_regions.len(),
            std::default::Default::default(),
        );
    let mut math_body: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::with_capacity_and_hasher(
        math_regions.len() * 2,
        std::default::Default::default(),
    );
    for (idx, reg) in math_regions.iter().enumerate() {
        math_start.insert(reg.range.start, idx);
        math_body.extend(reg.range.start + 1..reg.range.end);
    }
    let mut fold_lines: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut fold_bodies: Vec<Range<usize>> = Vec::new();
    for (rnge, folded) in &d.alert_fold_regions {
        if !*folded {
            continue;
        }
        fold_bodies.push(rnge.clone());
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    let fold_split = fold_bodies.len();
    for rnge in &d.heading_fold_bodies {
        fold_bodies.push(rnge.clone());
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    crate::prof::end("wy.fp.sets", _t_sets);
    Rc::new(FoldPlanSets {
        math_start,
        math_body,
        fold_lines,
        fold_bodies,
        fold_split,
    })
}

/// `FoldPlanSets`（marker 集合）构建 + 缓存。
fn fold_plan_sets(
    rd: &RefCell<WyRenderModel>,
    derived: &Rc<RefCell<WyDerived>>,
) -> Rc<FoldPlanSets> {
    let regions_ver = regions_ver(rd);
    let shift_ver = regions_shift_ver(rd);
    let fold_ver = fold_ver(rd);
    let skey = regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ shift_ver.wrapping_mul(0x296C_4EA9_6BE3_A5F1)
        ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
    if let Some((k, s)) = &*rd.borrow().fp_sets.borrow()
        && *k == skey
    {
        return s.clone();
    }
    let sets = build_fp_sets(derived);
    *rd.borrow_mut().fp_sets.borrow_mut() = Some((skey, sets.clone()));
    sets
}

/// deltas **增量平移**（B5 + M5-B3-1）：marker 集合来自缓存，deltas 平移旧计划。
fn fold_plan_deltas_shifted(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    derived: &Rc<RefCell<WyDerived>>,
    base: Rc<RefCell<BlockLayout>>,
    key: u64,
    marker_key: u64,
    sets: Rc<FoldPlanSets>,
) -> Option<Rc<FoldPlan>> {
    if !rd.borrow().struct_edit_pending.get() {
        return None;
    }
    let old_plan = {
        let rdg = rd.borrow();
        let old = rdg.fold_plan.borrow();
        let (_, cm, p) = old.as_ref()?;
        if *cm != marker_key {
            return None;
        }
        p.clone()
    };
    let ed_blk_old = rd.borrow().struct_blk_old.get();
    let blk_delta = rd.borrow().struct_blk_span_delta.get();
    let mut deltas: Vec<(usize, f32)> = Vec::with_capacity(old_plan.deltas.len() + 2);
    for &(ix, v) in old_plan.deltas.iter() {
        if ix < ed_blk_old {
            deltas.push((ix, v));
        } else if ix > ed_blk_old {
            deltas.push(((ix as i64 + blk_delta).max(0) as usize, v));
        }
    }
    let base_br = base.borrow();
    let n = base_br.n();
    let ed_blk_new = doc
        .block_index_for_line(struct_dirty_line(rd))
        .min(n.saturating_sub(1));
    let fold_lines = &sets.fold_lines;
    let math_body = &sets.math_body;
    let math_start = &sets.math_start;
    let member_delta = |ix: usize| -> Option<f32> {
        let m = doc.block_meta(ix)?;
        let s0 = m.src_line as usize;
        let s1 = (s0 + m.aux.max(1) as usize).min(doc.line_starts().len());
        let mut line = s0;
        while line < s1 {
            if fold_lines.contains(&line) || math_body.contains(&line) {
                return Some(-base_br.height(ix));
            }
            line += 1;
        }
        let mut line = s0;
        while line < s1 {
            if let Some(&ridx) = math_start.get(&line) {
                let card_h = {
                    let d = derived.borrow();
                    f32::from(crate::shape::math_card_height(rd, &d.math_regions[ridx]))
                };
                return Some(card_h - base_br.height(ix));
            }
            line += 1;
        }
        None
    };
    let lo = ed_blk_new.saturating_sub(1);
    let hi = (ed_blk_new + blk_delta.max(0) as usize + 2).min(n);
    for ix in lo..hi {
        if let Some(v) = member_delta(ix) {
            if let Some(pos) = deltas.iter().position(|&(b, _)| b == ix) {
                deltas.remove(pos);
            }
            deltas.push((ix, v));
        }
    }
    deltas.sort_unstable_by_key(|&(b, _)| b);
    deltas.dedup_by_key(|&mut (b, _)| b);
    let delta_sum: f32 = deltas.iter().map(|&(_, v)| v).sum();
    let delta_pref = build_delta_pref(&deltas);
    let plan = Rc::new(FoldPlan {
        sets,
        deltas: deltas.into_boxed_slice().into(),
        delta_pref,
        delta_sum,
    });
    rd.borrow_mut().struct_edit_pending.set(false);
    *rd.borrow_mut().fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
    Some(plan)
}

/// deltas **整建**：marker 集合 →「命中标记 + 块值」双数组。
fn fold_plan_deltas_full(
    rd: &RefCell<WyRenderModel>,
    derived: &Rc<RefCell<WyDerived>>,
    base: Rc<RefCell<BlockLayout>>,
    key: u64,
    marker_key: u64,
    sets: Rc<FoldPlanSets>,
) -> Rc<FoldPlan> {
    let math_regions = &derived.borrow().math_regions;
    let _t_dyn = crate::prof::begin();
    let base_br = base.borrow();
    let nb = base_br.n();
    let mut marked: Vec<bool> = vec![false; nb];
    let mut vals: Vec<f32> = vec![0.0; nb];
    for &srcline in sets.fold_lines.iter() {
        let ix = base_br.block_for_line(srcline);
        marked[ix] = true;
        vals[ix] = -base_br.height(ix);
    }
    for &srcline in sets.math_body.iter() {
        let ix = base_br.block_for_line(srcline);
        marked[ix] = true;
        vals[ix] = -base_br.height(ix);
    }
    for (&srcline, &ridx) in sets.math_start.iter() {
        let ix = base_br.block_for_line(srcline);
        if !marked[ix] {
            let card_h = f32::from(crate::shape::math_card_height(rd, &math_regions[ridx]));
            marked[ix] = true;
            vals[ix] = card_h - base_br.height(ix);
        }
    }
    crate::prof::end("wy.fp.dyn", _t_dyn);
    let _t_tail = crate::prof::begin();
    let mut deltas_vec: Vec<(usize, f32)> = Vec::new();
    for ix in 0..nb {
        if marked[ix] {
            deltas_vec.push((ix, vals[ix]));
        }
    }
    let delta_sum: f32 = deltas_vec.iter().map(|&(_, v)| v).sum();
    crate::prof::end("wy.fp.tail", _t_tail);
    let delta_pref = build_delta_pref(&deltas_vec);
    let plan = Rc::new(FoldPlan {
        sets,
        deltas: deltas_vec.into_boxed_slice().into(),
        delta_pref,
        delta_sum,
    });
    rd.borrow_mut().struct_edit_pending.set(false);
    *rd.borrow_mut().fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
    plan
}

/// 折叠/重排计划（缓存）：marker 集合与 `deltas` 分层缓存，全部与可视带无关且不含 caret。
pub fn fold_plan(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: Rc<RefCell<BlockLayout>>,
    epoch: u64,
    mgen: u64,
) -> Rc<FoldPlan> {
    let regions_ver = regions_ver(rd);
    let fold_ver = fold_ver(rd);
    let key = epoch
        ^ regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
    let marker_key = regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
    if let Some((k, _m, p)) = &*rd.borrow().fold_plan.borrow()
        && *k == key
    {
        return p.clone();
    }
    let derived = derived(rd, doc);
    let sets = fold_plan_sets(rd, &derived);
    if let Some(plan) = fold_plan_deltas_shifted(
        rd,
        doc,
        &derived,
        base.clone(),
        key,
        marker_key,
        sets.clone(),
    ) {
        return plan;
    }
    fold_plan_deltas_full(rd, &derived, base, key, marker_key, sets)
}

/// caret 所在区域「展开显示源码」（reveal-on-caret）覆盖，按带现算。
pub fn fold_reveal(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    caret_line: usize,
    plan: &FoldPlan,
    base: &RefCell<BlockLayout>,
) -> RevealPlan {
    let mut reveal = RevealPlan::empty();
    let base_br = base.borrow();
    let block_ix_of =
        |srcline: usize| -> usize { base_br.block_index_for_src_line(srcline).unwrap_or(0) };
    let zero_ix = |zeroed: &mut Vec<usize>, ix: usize| {
        if !zeroed.contains(&ix) {
            zeroed.push(ix);
        }
    };
    {
        let d = derived(rd, doc);
        let d = d.borrow();
        if let Some(ridx) = region_containing(&d.math_regions, |r| r.range.clone(), caret_line, true)
        {
            let reg = &d.math_regions[ridx];
            let rng = reg.range.clone();
            reveal.ranges.push(rng.clone());
            zero_ix(&mut reveal.zeroed, block_ix_of(rng.start));
            for l in rng.start + 1..rng.end {
                zero_ix(&mut reveal.zeroed, block_ix_of(l));
            }
        }
    }
    if let Some(rnge) = folded_body_at(&plan.sets.fold_bodies, plan.sets.fold_split, caret_line) {
        let rng = rnge.clone();
        reveal.ranges.push(rng.clone());
        for l in rng.start + 1..rng.end {
            zero_ix(&mut reveal.zeroed, block_ix_of(l));
        }
    }
    if !reveal.zeroed.is_empty() {
        for &ix in &reveal.zeroed {
            if let Some((_, v)) = plan.deltas.iter().find(|(i, _)| *i == ix) {
                reveal.reveal_delta_sum += v;
            }
        }
    }
    reveal
}

/* ---------- 图片缓存失效 / 尺寸写入 / 稀疏覆写（原宿主 image.rs 方法逐字迁入） ---------- */

/// 从当前 Doc 的块索引**全量**构建「`src → 块下标」反向表（O(块数) 一次，缓存）。
pub fn build_image_src_blocks(rd: &RefCell<WyRenderModel>, doc: &Doc) -> ImageSrcBlocks {
    let mut map: std::collections::HashMap<gpui::SharedString, Vec<usize>> =
        std::collections::HashMap::new();
    for b in 0..doc.block_count() {
        let Some(m) = doc.block_meta(b) else {
            continue;
        };
        if m.kind != RowKind::Image {
            continue;
        }
        let src = markdown::parse::parse_image_line(doc.line_str(m.src_line as usize).trim())
            .map(|(_, s)| s)
            .unwrap_or_default();
        map.entry(src.into()).or_default().push(b);
    }
    (content_gen(rd), map)
}

/// 图片宽/尺寸变化后，**定向作废**含该 `src` 图片行的行整形缓存条目。
pub fn invalidate_image_row_cache(rd: &RefCell<WyRenderModel>, doc: &Doc, src: &str) {
    let cur_gen = content_gen(rd);
    let need_rebuild = {
        let rdg = rd.borrow();
        let cached = rdg.image_src_blocks.borrow();
        cached.as_ref().is_none_or(|(g, _)| *g != cur_gen)
    };
    if need_rebuild {
        let built = build_image_src_blocks(rd, doc);
        *rd.borrow_mut().image_src_blocks.borrow_mut() = Some(built);
    }
    let ids: Vec<u64> = {
        let rdg = rd.borrow();
        let cached = rdg.image_src_blocks.borrow();
        let Some((_, map)) = cached.as_ref() else {
            return;
        };
        let Some(block_indices) = map.get(src) else {
            return;
        };
        if block_indices.is_empty() {
            return;
        }
        block_indices
            .iter()
            .map(|&b| block_content_id(doc, b))
            .collect()
    };
    rd.borrow_mut().shape_rows.borrow_mut().invalidate_ids(&ids);
}

/// 记录 `src` 的显示尺寸；尺寸变化自增 `images_gen` 使帧缓存失效，并把「含该图的块高」
/// 收录进 `pending_image_srcs`（下一帧 `build_frame` 基表复用路径稀疏覆写）。
pub fn set_image_size(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    src: gpui::SharedString,
    size: Option<(f32, f32)>,
) {
    let changed = rd.borrow().image_sizes.borrow().get(&src).copied() != Some(size);
    if changed {
        invalidate_image_row_cache(rd, doc, &src);
        rd.borrow_mut().image_sizes.borrow_mut().insert(src.clone(), size);
        {
            let rdg = rd.borrow_mut();
            rdg.images_gen
                .set(rdg.images_gen.get().wrapping_add(1));
        }
        rd.borrow_mut().pending_image_srcs.borrow_mut().insert(src);
    }
}

/// 记录用户拖拽调整的图片宽（px，已钳制）；变化 → `images_gen` 自增 + 收录稀疏覆写。
pub fn set_image_width(rd: &RefCell<WyRenderModel>, doc: &Doc, src: gpui::SharedString, w: f32) {
    let clamped = w
        .clamp(
            config::markdown::IMAGE_MIN_W,
            config::markdown::IMAGE_MAX_W,
        )
        .max(1.);
    if rd.borrow().image_widths.borrow().get(&src).copied() != Some(clamped) {
        invalidate_image_row_cache(rd, doc, &src);
        rd.borrow_mut()
            .image_widths
            .borrow_mut()
            .insert(src.clone(), clamped);
        {
            let rdg = rd.borrow_mut();
            rdg.images_gen
                .set(rdg.images_gen.get().wrapping_add(1));
        }
        rd.borrow_mut().pending_image_srcs.borrow_mut().insert(src);
    }
}

/// **图片行高稀疏定向覆写**（M3 A4）：`build_frame` 在「基础布局复用」路径把
/// `pending_image_srcs` 收录的 src 涉及的所有**含图块**基础高做 Fenwick 点更新。
pub fn patch_pending_image_heights(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: &BlockLayoutRef,
    avail: f32,
    line_h: Pixels,
) {
    let srcs: Vec<gpui::SharedString> = rd
        .borrow_mut()
        .pending_image_srcs
        .borrow_mut()
        .drain()
        .collect();
    if srcs.is_empty() {
        return;
    }
    // 陈旧（`content_gen` 变化）才整表重建反向索引一次；命中后 O(1) 取块集。
    // ⚠️ 重建判定借用收进内层块（同 `invalidate_image_row_cache`），块末随 `rd` 释放。
    let cur_gen = content_gen(rd);
    let need_rebuild = {
        let rdg = rd.borrow();
        let cached = rdg.image_src_blocks.borrow();
        cached.as_ref().is_none_or(|(g, _)| *g != cur_gen)
    };
    if need_rebuild {
        let built = build_image_src_blocks(rd, doc);
        *rd.borrow_mut().image_src_blocks.borrow_mut() = Some(built);
    }
    let rdg = rd.borrow();
    let cached = rdg.image_src_blocks.borrow();
    let Some((_, map)) = cached.as_ref() else {
        return;
    };
    let mut b = base.borrow_mut();
    for src in &srcs {
        let Some(ixs) = map.get(src) else {
            continue;
        };
        for &blk in ixs {
            if blk >= b.n() {
                continue;
            }
            let h = crate::shape::image_block_base_height(rd, src, avail, line_h);
            b.set(blk, h);
        }
    }
}

/* ---------- 帧装配主线（自宿主 frame/mod.rs 逐字迁入） ---------- */

/// 冷启动首次 shape 回退字形（❗⚠✅ 等）时 DWrite 可能给「错误回退字体的非零占位字形」
/// （glyph id≠0，逃过 `.notdef` 检测）→ 渲染方框且会被直接缓存。用它识别这些行。
fn row_uses_fallback(ts: &gpui::WindowTextSystem, _base: &gpui::Font, srow: &ShapedRow) -> bool {
    if !srow.has_non_ascii {
        return false;
    }
    let mut known = false;
    let mut unknown = false;
    for wl in srow.wrapped.iter() {
        for r in &wl.unwrapped_layout.runs {
            if ts.get_font_for_id(r.font_id).is_some() {
                known = true;
            } else {
                unknown = true;
            }
            if known && unknown {
                return true;
            }
        }
    }
    false
}

/// 诊断：`WY_NOTDEF_DEBUG=1` 时输出含非 ASCII 字形的行的真实整形结果。
static LAST_SPECIAL_ROW: std::sync::Mutex<Option<(usize, String)>> =
    std::sync::Mutex::new(None);
fn debug_special_row(
    host: &dyn WyRenderHost,
    window: &Window,
    rl: usize,
    srow: &ShapedRow,
    base_font: &gpui::Font,
) {
    if std::env::var_os("WY_NOTDEF_DEBUG").is_none() {
        return;
    }
    let txt = host.doc().line_str(rl);
    if txt.bytes().all(|b| b.is_ascii()) {
        return;
    }
    let ts = window.text_system();
    let base_id = ts.resolve_font(base_font);
    let mut sig = String::new();
    let mut parts = Vec::new();
    for wl in srow.wrapped.iter() {
        for run in &wl.unwrapped_layout.runs {
            let fam = ts
                .get_font_for_id(run.font_id)
                .map(|f| f.family.to_string())
                .unwrap_or_else(|| format!("#{}", run.font_id.0));
            let fb = if run.font_id != base_id { "*FB" } else { "" };
            let mut glyphs = String::new();
            let mut last_idx = None;
            for g in &run.glyphs {
                if last_idx == Some(g.index) {
                    continue;
                }
                last_idx = Some(g.index);
                if !glyphs.is_empty() {
                    glyphs.push(',');
                }
                glyphs.push_str(&format!("{}:{}", g.index, g.id.0));
            }
            sig.push_str(&format!("|{}:{}:{}", run.font_id.0, fam, glyphs));
            parts.push(format!(
                "{} id={} emo={}{} [{}]",
                fam,
                run.font_id.0,
                run.glyphs.iter().any(|g| g.is_emoji),
                fb,
                glyphs
            ));
        }
    }
    {
        let mut last = LAST_SPECIAL_ROW.lock().unwrap();
        if last.as_ref().is_some_and(|(lr, ls)| *lr == rl && *ls == sig) {
            return;
        }
        *last = Some((rl, sig));
    }
    let sample: String = txt.chars().take(40).collect();
    notdef_debug(format_args!(
        "row rl={rl} base=#{} {}",
        base_id.0,
        parts.join(" | ")
    ));
    notdef_debug(format_args!("  text={sample:?}"));
}

/// **Pass 1**（仅结构性变更时）：全块**基础高** + 起始源码行。三段式增量。
#[allow(clippy::too_many_arguments)]
pub fn pass1_base_layout(
    doc: &Doc,
    rd: &RefCell<WyRenderModel>,
    n: usize,
    base: &Rc<RefCell<BlockLayout>>,
    old_layout: Option<&Rc<RefCell<BlockLayout>>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) {
    let _t_p1 = crate::prof::begin();
    let dirty_line = struct_dirty_line(rd);
    let old_end_line = old_edit_end_line(rd);
    let line_delta = edit_line_delta(rd);
    let doc_lines = doc.line_starts().len();
    let p = {
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let sl = doc.block_meta(mid).map(|m| m.src_line as usize).unwrap_or(doc_lines);
            if sl <= dirty_line {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let keep = p.saturating_sub(1);
    let old_holder = old_layout.cloned();
    let old_n = old_holder.as_ref().map(|rc| rc.borrow().n());
    let j = match &old_holder {
        Some(rc) => {
            let ob = rc.borrow();
            let mut lo = 0usize;
            let mut hi = ob.n();
            while lo < hi {
                let mid = (lo + hi) / 2;
                if ob.src_line(mid) as usize <= old_end_line {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        }
        None => 0,
    };
    let tail_src = match &old_holder {
        Some(rc) => {
            let ob = rc.borrow();
            if j < ob.n() {
                ob.src_line(j) as i64 + line_delta
            } else {
                doc_lines as i64
            }
        }
        None => doc_lines as i64,
    };
    let k = if old_n.is_none() {
        n
    } else if keep < n {
        let mut lo = keep;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let sl = doc.block_meta(mid).map(|m| m.src_line as i64).unwrap_or(i64::MAX);
            if sl >= tail_src {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    } else {
        keep
    };
    let mid_start = old_holder
        .as_ref()
        .map(|rc| keep.min(rc.borrow().n()))
        .unwrap_or(0);
    let est_block = |i: usize| -> (f32, u32) {
        let meta = if let Some(m) = doc.block_meta(i) {
            m
        } else {
            return (f32::from(line_h), 0);
        };
        let srcline = meta.src_line as usize;
        let h: f32 = if meta.kind == RowKind::Code
            && doc.line_str(srcline).trim_start().starts_with("```mermaid")
        {
            let mut src = String::new();
            for k in 0..meta.aux {
                if k > 0 {
                    src.push('\n');
                }
                src.push_str(&doc.line_str(srcline + 1 + k as usize));
            }
            f32::from(mermaid_card_height(&src)) * mermaid_zoom_of(rd, srcline)
        } else if meta.kind == RowKind::Image {
            let src = markdown::parse::parse_image_line(doc.line_str(srcline).trim())
                .map(|(_, s)| s)
                .unwrap_or_default();
            image_block_base_height(rd, &src, avail, line_h)
        } else {
            let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
            f32::from(estimate_height(&meta, chars, ascii, theme, avail, line_h))
        };
        (h, meta.src_line)
    };
    let mut middle: Vec<(f32, u32)> = Vec::with_capacity(k.saturating_sub(mid_start));
    for i in mid_start..k {
        middle.push(est_block(i));
    }
    let tail_ok = matches!(old_n, Some(n_old) if k <= n && j <= n_old && n_old - j == n - k && keep <= n_old);
    if tail_ok {
        let old_ref = old_holder.as_ref().expect("tail_ok 蕴含旧布局在场").borrow();
        base.borrow_mut().apply_structural_edit(&old_ref, keep, &middle, j, line_delta, n);
        crate::prof::end("wy.pass1", _t_p1);
        return;
    }
    let (mut heights, mut src_lines) = if let Some(rc) = &old_holder {
        rc.borrow().flat_range(0, mid_start)
    } else {
        (Vec::with_capacity(n), Vec::with_capacity(n))
    };
    heights.extend(middle.iter().map(|&(h, _)| h));
    src_lines.extend(middle.iter().map(|&(_, s)| s));
    for i in heights.len()..n {
        let (h, sl) = est_block(i);
        heights.push(h);
        src_lines.push(sl);
    }
    debug_assert_eq!(heights.len(), n, "Pass1 三段式产出的块数与新索引一致");
    base.borrow_mut().rebuild_from_flat(heights, src_lines);
    crate::prof::end("wy.pass1", _t_p1);
}

/// —— Step4-①：首帧 Pass1 预建（后台算输入 + 首帧纯算术物化）——
pub fn build_prebuild_pass1(doc: &Doc) -> PrebuildPass1 {
    let _t = crate::prof::begin();
    let n = doc.block_count();
    let mut blocks = Vec::with_capacity(n);
    let mut need_chars: Vec<(usize, usize)> = Vec::new();
    for i in 0..n {
        let Some(meta) = doc.block_meta(i) else { break };
        let srcline = meta.src_line as usize;
        let mut pb = PrebuildBlock {
            src_line: meta.src_line,
            kind: meta.kind,
            aux: meta.aux,
            chars: 0,
            ascii: true,
            mermaid: None,
            image_src: None,
        };
        match meta.kind {
            RowKind::Code => {
                if doc.line_str(srcline).trim_start().starts_with("```mermaid") {
                    let mut src = String::new();
                    for k in 0..meta.aux {
                        if k > 0 {
                            src.push('\n');
                        }
                        src.push_str(&doc.line_str(srcline + 1 + k as usize));
                    }
                    pb.mermaid = Some(gpui::SharedString::from(src));
                }
            }
            RowKind::Image => {
                pb.image_src = Some(
                    markdown::parse::parse_image_line(doc.line_str(srcline).trim())
                        .map(|(_, s)| gpui::SharedString::from(s.to_string()))
                        .unwrap_or_default(),
                );
            }
            RowKind::Table => {}
            _ => need_chars.push((i, srcline)),
        }
        blocks.push(pb);
    }
    fill_prebuild_chars(doc, &mut blocks, &need_chars);
    crate::prof::end("wy.prebuild", _t);
    PrebuildPass1 { blocks }
}

/// 单趟 chunk 顺序扫描填 `need_chars` 各块首行的 `(chars, ascii)`。
fn fill_prebuild_chars(doc: &Doc, blocks: &mut [PrebuildBlock], need: &[(usize, usize)]) {
    if need.is_empty() {
        return;
    }
    let rope = doc.rope();
    let mut ni = 0usize;
    let (mut bi, mut tgt) = need[ni];
    let mut cur_line = 0usize;
    let mut started = false;
    let mut line_chars = 0u32;
    let mut line_ascii = true;
    for chunk in rope.chunks() {
        let bytes = chunk.as_bytes();
        let mut j = 0usize;
        let m = bytes.len();
        while j < m {
            if !started && cur_line == tgt {
                started = true;
                line_chars = 0;
                line_ascii = true;
            }
            let b = bytes[j];
            if started {
                if b == b'\n' {
                    blocks[bi].chars = line_chars;
                    blocks[bi].ascii = line_ascii;
                    ni += 1;
                    if ni >= need.len() {
                        return;
                    }
                    (bi, tgt) = need[ni];
                    started = false;
                    cur_line += 1;
                    j += 1;
                    continue;
                }
                if b == b'\r' {
                    j += 1;
                    continue;
                }
                line_chars += 1;
                if b >= 0x80 {
                    line_ascii = false;
                    let l = if b < 0xC0 {
                        1
                    } else if b < 0xE0 {
                        2
                    } else if b < 0xF0 {
                        3
                    } else {
                        4
                    };
                    j += l;
                } else {
                    j += 1;
                }
            } else {
                if b == b'\n' {
                    cur_line += 1;
                }
                j += 1;
            }
        }
    }
    if started && ni < need.len() {
        blocks[bi].chars = line_chars;
        blocks[bi].ascii = line_ascii;
    }
}

/// 单个预建块的估算高——与 [`pass1_base_layout`] 的 `est_block` **同一公式**。
pub fn prebuild_block_height(
    rd: &RefCell<WyRenderModel>,
    pb: &PrebuildBlock,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    if let Some(src) = &pb.mermaid {
        f32::from(mermaid_card_height(src))
    } else if pb.kind == RowKind::Image {
        let src: &str = pb.image_src.as_deref().unwrap_or_default();
        image_block_base_height(rd, src, avail, line_h)
    } else {
        let meta = BlockMeta {
            src_line: pb.src_line,
            byte: 0,
            kind: pb.kind,
            raw_len: 0,
            aux: pb.aux,
        };
        f32::from(estimate_height(&meta, pb.chars as usize, pb.ascii, theme, avail, line_h))
    }
}

/// 用预建输入在**首帧**把整篇基础布局物化（O(n) 纯算术，无 doc 行访问）。成功物化后
/// **清空预建槽**（一次性：之后任何重建走既有 `pass1_base_layout` / 稀疏增量）。
/// 调用方（[`build_frame`]）须先判守卫（`struct_ver==0 && zgen==0 && 块数齐`）。
pub fn pass1_from_prebuild(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> bool {
    let _t = crate::prof::begin();
    {
        let rdg = rd.borrow();
        let b = rdg.prebuild_slot.borrow();
        let Some(p) = b.as_ref() else { return false };
        if p.len() != doc.block_count() {
            return false;
        }
        let mut heights = Vec::with_capacity(p.len());
        let mut src_lines = Vec::with_capacity(p.len());
        for pb in &p.blocks {
            heights.push(prebuild_block_height(rd, pb, theme, avail, line_h));
            src_lines.push(pb.src_line);
        }
        base.borrow_mut().rebuild_from_flat(heights, src_lines);
    }
    *rd.borrow_mut().prebuild_slot.borrow_mut() = None;
    crate::prof::end("wy.pass1_prebuilt", _t);
    true
}

/// 在可视带内二分定位 [i0, i1) 块区间。
fn find_band_range(
    base: &Rc<RefCell<BlockLayout>>,
    deltas: &[(usize, f32)],
    delta_pref: &[f32],
    band_lo: f32,
    band_hi: f32,
    n: usize,
) -> (usize, usize) {
    let _t_bis = crate::prof::begin();
    debug_assert_eq!(delta_pref.len(), deltas.len() + 1, "前缀和表须与增量表对齐");
    let eff_top = |i: usize| -> f32 {
        let b = base.borrow();
        let k = deltas.partition_point(|&(ix, _)| ix < i);
        b.top(i) + delta_pref[k]
    };
    let count_true = |pred: &dyn Fn(usize) -> bool| -> usize {
        let mut l = 0usize;
        let mut r = n + 1;
        while l < r {
            let m = (l + r) / 2;
            if pred(m) {
                l = m + 1;
            } else {
                r = m;
            }
        }
        l
    };
    let i0 = count_true(&|k: usize| eff_top(k) <= band_lo).saturating_sub(1);
    let i1 = count_true(&|k: usize| eff_top(k) < band_hi).min(n);
    crate::prof::end("wy.bf.bisect", _t_bis);
    (i0, i1)
}

/// **Pass 2**：带内非折叠/非数学块 shape，真实高 `set` 回基表（带外沿用收敛真值）。
#[allow(clippy::too_many_arguments)]
fn pass2_shape_band(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    window: &mut Window,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    theme: &WyTheme,
    base_font: &gpui::Font,
    width: Pixels,
    avail: f32,
    i0: usize,
    i1: usize,
    shape_key: u64,
    caret_line: usize,
    plan: &FoldPlan,
    reveal: &RevealPlan,
) -> Vec<(usize, ShapedRow)> {
    let _t_p2 = crate::prof::begin();
    let fold_lines = &plan.sets.fold_lines;
    let math_body = &plan.sets.math_body;
    let math_start = &plan.sets.math_start;
    let mut shaped: Vec<(usize, ShapedRow)> = Vec::new();
    let mut i = i0;
        while i < i1 {
            let srcline = base.borrow().src_line(i) as usize;
            let blocked = !reveal.contains(srcline)
                && (fold_lines.contains(&srcline)
                    || math_body.contains(&srcline)
                    || math_start.contains_key(&srcline));
            if blocked {
                i += 1;
                continue;
            }
            let lo = i;
            while i < i1 {
                let s = base.borrow().src_line(i) as usize;
                if !reveal.contains(s)
                    && (fold_lines.contains(&s)
                        || math_body.contains(&s)
                        || math_start.contains_key(&s))
                {
                    break;
                }
                i += 1;
            }
            let hi = i;
            let (need_row, mut hit_map) =
                band_hit_test(rd, doc, base, lo, hi, caret_line, shape_key);
            let mut rows_map = band_parse_rows(doc, &need_row);
            let _t_shape = crate::prof::begin();
            for k in lo..hi {
                let cid = block_content_id(doc, k);
                let mut srow = if let Some(rc) = hit_map.remove(&k) {
                    (*rc).clone()
                } else if is_giant_code_block(doc, base, k) {
                    giant_fast_row(rd, doc, base, k, shape_key)
                } else if let Some(last) = cooling_row(rd, k) {
                    window.refresh();
                    #[cfg(feature = "test-mode")]
                    bump_coolings(rd);
                    (*last).clone()
                } else {
                    let r = rows_map.remove(&k).expect("未命中 / caret 行应已被物化");
                    let is_caret = !matches!(r.kind, IrKind::Code | IrKind::Table | IrKind::Image)
                        && caret_line == r.src_line;
                    let rl = r.src_line;
                    #[cfg(feature = "test-mode")]
                    bump_reshapes(rd);
                    let srow = crate::shape::shape_docrow(r, rd, host, window, width, avail, theme, base_font);
                    if shaped_row_has_notdef(&srow) {
                        if notdef_settled(rd, k) {
                            srow
                        } else if cool_start(rd, k) {
                            notdef_debug(format_args!(
                                "pass2 give-up k={} src={} caret={is_caret}",
                                k, rl
                            ));
                            if is_caret {
                                srow
                            } else {
                                let rc = rd.borrow_mut().shape_rows.borrow_mut().put(cid, shape_key, srow);
                                (*rc).clone()
                            }
                        } else {
                            window.refresh();
                            cool_keep_last(rd, k, std::rc::Rc::new(srow.clone()));
                            notdef_debug(format_args!(
                                "pass2 notdef k={} src={} caret={is_caret} -> cooling",
                                k, rl
                            ));
                            srow
                        }
                    } else if row_uses_fallback(window.text_system(), base_font, &srow)
                        && !fallback_verified(rd, k)
                    {
                        mark_fallback_verified(rd, k);
                        verify_cool(rd, k);
                        window.refresh();
                        cool_keep_last(rd, k, std::rc::Rc::new(srow.clone()));
                        notdef_debug(format_args!(
                            "pass2 fallback-verify k={} src={} caret={is_caret}",
                            k, rl
                        ));
                        srow
                    } else {
                        clear_notdef(rd, k);
                        if is_caret && !srow.extra.inline_maths.is_empty() {
                            srow
                        } else {
                            let rc = rd.borrow_mut().shape_rows.borrow_mut().put(cid, shape_key, srow);
                            (*rc).clone()
                        }
                    }
                };
                let srcl = base.borrow().src_line(k) as usize;
                let srcl_next = {
                    let b = base.borrow();
                    if k + 1 < b.n() {
                        b.src_line(k + 1) as usize
                    } else {
                        doc.line_count().max(srcl + 1)
                    }
                };
                srow.src_line = srcl;
                srow.src_lines = srcl..srcl_next.max(srcl + 1);
                debug_special_row(host, window, srcl, &srow, base_font);
                base.borrow_mut().set(k, f32::from(srow.height));
                shaped.push((k, srow));
            }
            crate::prof::end("wy.bf.shape", _t_shape);
        }
    crate::prof::end("wy.pass2", _t_p2);
    shaped
}

/// A1+「命中先行」（内容寻址）：整带内判哪些块不需要物化 DocRow。
#[allow(clippy::type_complexity)]
fn band_hit_test(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    lo: usize,
    hi: usize,
    caret_line: usize,
    shape_key: u64,
) -> (Vec<usize>, std::collections::HashMap<usize, std::rc::Rc<ShapedRow>>) {
    let gen_changed = rd.borrow().frame.borrow().as_ref().is_some_and(|f| {
        (f.rgen, f.igen, f.mgen, f.zgen)
            != (
                content_gen(rd),
                images_gen(rd),
                math_gen(rd),
                mermaid_zoom_gen(rd),
            )
    });
    let rdg = rd.borrow_mut();
    let mut row_cache = rdg.shape_rows.borrow_mut();
    let notdef = &rdg.notdef_retry;
    let mut need_row: Vec<usize> = Vec::new();
    let mut hit_map: std::collections::HashMap<usize, std::rc::Rc<ShapedRow>> =
        std::collections::HashMap::new();
    for k in lo..hi {
        if is_giant_code_block(doc, base, k) {
            continue;
        }
        let srcline = base.borrow().src_line(k) as usize;
        let cid = block_content_id(doc, k);
        if caret_line == srcline {
            need_row.push(k);
        } else if let Some(rc) = row_cache.get(cid, shape_key) {
            if shaped_row_has_notdef(&rc) && !notdef.borrow().settled.contains(&k) {
                need_row.push(k);
            } else {
                hit_map.insert(k, rc);
            }
        } else {
            if gen_changed && row_cache.key_mismatch(cid, shape_key) {
                notdef.borrow_mut().clear_stale(k);
            }
            need_row.push(k);
        }
    }
    (need_row, hit_map)
}

/// 只物化 miss / caret 块（连续段合并为一次 `rows_in_block_window` 批量 parse）。
fn band_parse_rows(doc: &Doc, need_row: &[usize]) -> std::collections::HashMap<usize, DocRow> {
    let _t_parse = crate::prof::begin();
    let mut rows_map: std::collections::HashMap<usize, DocRow> =
        std::collections::HashMap::new();
    let mut ni = 0usize;
    while ni < need_row.len() {
        let lo2 = need_row[ni];
        let mut hi2 = lo2;
        ni += 1;
        while ni < need_row.len() && need_row[ni] == hi2 + 1 {
            hi2 += 1;
            ni += 1;
        }
        for (off, r) in doc.rows_in_block_window(lo2, hi2).into_iter().enumerate() {
            rows_map.insert(lo2 + off, r);
        }
    }
    crate::prof::end("wy.bf.blockparse", _t_parse);
    rows_map
}

/// I4 巨型代码块**快速路径行**。
fn giant_fast_row(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
    shape_key: u64,
) -> ShapedRow {
    let cid = block_content_id(doc, k);
    if let Some(rc) = rd.borrow_mut().shape_rows.borrow_mut().get(cid, shape_key) {
        return (*rc).clone();
    }
    let stale = {
        let rdg = rd.borrow();
        let pw = rdg.prewarm.borrow();
        pw.giant_stale(cid)
    };
    if let Some(rc) = stale {
        rd.borrow_mut().prewarm.borrow_mut().register_giant(k);
        return (*rc).clone();
    }
    rd.borrow_mut().prewarm.borrow_mut().register_giant(k);
    giant_placeholder_row(rd, doc, base, k)
}

/// I4 巨块估计高占位。
fn giant_placeholder_row(
    rd: &RefCell<WyRenderModel>,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
) -> ShapedRow {
    let (src_line, height) = {
        let b = base.borrow();
        (b.src_line(k) as usize, b.height(k))
    };
    let next = {
        let b = base.borrow();
        if k + 1 < b.n() {
            b.src_line(k + 1) as usize
        } else {
            doc.line_count().max(src_line + 1)
        }
    };
    ShapedRow {
        src_line,
        src_lines: src_line..next.max(src_line + 1),
        wrapped: std::rc::Rc::new(Vec::new()),
        has_non_ascii: false,
        sub_heights: Vec::new(),
        byte_map: std::rc::Rc::new(Vec::new()),
        editable: false,
        multi_offsets: Vec::new(),
        code_ranges: Vec::new(),
        lh: rd.borrow().theme.code_line(),
        x_off: px(config::markdown::CONTENT_EDGE_PAD_X_PX),
        align_offsets: Vec::new(),
        height: px(height),
        top: px(0.),
        kind: IrKind::Code,
        quoted: false,
        table: None,
        table_edit: None,
        image: None,
        mm: None,
        extra: RowExtra::default(),
    }
}

/// **Pass 3**：组装带内行（有效 top = 基础前缀 + 动态增量前缀）。
#[allow(clippy::too_many_arguments)]
fn pass3_assemble_rows(
    base: &Rc<RefCell<BlockLayout>>,
    deltas: &[(usize, f32)],
    delta_sum: f32,
    shaped: Vec<(usize, ShapedRow)>,
    i0: usize,
    i1: usize,
    plan: &FoldPlan,
    math_regions: &[MathRegion],
    reveal: &RevealPlan,
) -> (Vec<ShapedRow>, f32) {
    let _t_pass3 = crate::prof::begin();
    let math_start = &plan.sets.math_start;
    let math_body = &plan.sets.math_body;
    let fold_lines = &plan.sets.fold_lines;
    let mut rows: Vec<ShapedRow> = Vec::new();
    let mut shaped_iter = shaped.into_iter().peekable();
    let total = {
        let b = base.borrow();
        b.total()
    } + delta_sum;
    let mut di = 0usize;
    let mut acc = 0f32;
    for i in i0..i1 {
        while di < deltas.len() && deltas[di].0 < i {
            acc += deltas[di].1;
            di += 1;
        }
        let srcline = base.borrow().src_line(i) as usize;
        let b = base.borrow();
        let d_here = if di < deltas.len() && deltas[di].0 == i {
            Some(deltas[di].1)
        } else {
            None
        };
        let h = b.height(i) + d_here.unwrap_or(0.);
        let top = b.top(i) + acc;
        drop(b);
        if h <= 0. {
            continue;
        }
        let shown_source = reveal.contains(srcline);
        let folded_here = !shown_source
            && (fold_lines.contains(&srcline) || math_body.contains(&srcline));
        if !folded_here
            && !shown_source
            && let Some(&ridx) = math_start.get(&srcline)
        {
            let reg = &math_regions[ridx];
            rows.push(build_math_card_row(reg, srcline, px(h), px(top)));
            continue;
        }
        if shaped_iter.peek().map(|(k, _)| *k) == Some(i) {
            let (_, mut srow) = shaped_iter.next().unwrap();
            srow.top = px(top);
            rows.push(srow);
        }
    }
    crate::prof::end("wy.pass3", _t_pass3);
    (rows, total)
}

/// 构建一帧：**窗口化**——仅 `band` 内的块被物化 + shape，带外估算占位。
pub fn block_content_id(doc: &Doc, block: usize) -> u64 {
    let Some(m) = doc.block_meta(block) else {
        return block as u64;
    };
    use std::hash::Hasher as _;
    let mut h = rustc_hash::FxHasher::default();
    let kind = match m.kind {
        RowKind::Paragraph => 1u64,
        RowKind::Heading(_) => 2u64,
        RowKind::Code => 3u64,
        RowKind::Quote => 4u64,
        RowKind::Bullet => 5u64,
        RowKind::Rule => 6u64,
        RowKind::Table => 7u64,
        RowKind::Image => 8u64,
        RowKind::Blank => 9u64,
    };
    h.write_u64(kind);
    h.write_u64(m.aux as u64);
    let src0 = m.src_line as usize;
    h.write(doc.line_str(src0).as_bytes());
    if m.aux > 1 {
        let srcn = (src0 + m.aux as usize - 1).min(doc.line_starts().len().saturating_sub(1));
        h.write(doc.line_str(srcn).as_bytes());
    }
    h.finish()
}

// ═══ I4 巨型代码块「整形跳过 → 静止权威整形」闸门 ═══
pub const GIANT_SPAN_LINES: usize = 64;
pub const GIANT_SPAN_BYTES: usize = 4096;

/// 块跨度 `(覆盖源码行数, 字节跨度)`。
pub fn block_span(
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
) -> (usize, i64) {
    let base_ref = base.borrow();
    let s0 = if k < base_ref.n() {
        base_ref.src_line(k) as usize
    } else {
        0
    };
    let s1 = if k + 1 < base_ref.n() {
        base_ref.src_line(k + 1) as usize
    } else {
        s0 + 1
    };
    let span_bytes = doc
        .block_meta(k)
        .and_then(|m| doc.block_meta(k + 1).map(|n| n.byte as i64 - m.byte as i64))
        .unwrap_or(0);
    (s1.saturating_sub(s0), span_bytes)
}

/// **巨型代码块**判定：`kind == Code` 且跨度超阈值。
pub fn is_giant_code_block(
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
) -> bool {
    let (span_lines, span_bytes) = block_span(doc, base, k);
    doc.block_meta(k).is_some_and(|m| m.kind == RowKind::Code)
        && (span_lines > GIANT_SPAN_LINES || span_bytes > GIANT_SPAN_BYTES as i64)
}

/// 居中阅读栏的**水平移位**（px）：`(元素可用宽 − 帧布局宽) ÷ 2`（原宿主
/// `WyEditor::reading_col_shift` 逐字迁入：`fw` 读 `rd.frame` 宽）。
pub fn reading_col_shift(rd: &RefCell<WyRenderModel>, elem_w: f32) -> f32 {
    let fw = rd
        .borrow()
        .frame
        .borrow()
        .as_ref()
        .map(|f| f32::from(f.width))
        .unwrap_or(elem_w);
    ((elem_w - fw) / 2.).max(0.)
}

pub fn build_frame(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    window: &mut Window,
    width: Pixels,
    band: (f32, f32),
) -> Frame {
    let _t_bf = crate::prof::begin();
    let theme = rd.borrow().theme.clone();
    let theme = &theme;
    let line_h = theme.para_line();
    let base_font = theme.font.clone();
    let doc = host.doc();
    let n = doc.block_count();
    let padding = px(config::markdown::CONTENT_EDGE_PAD_X_PX);
    let rgen = content_gen(rd);

    if n == 0 {
        return empty_frame(rd, line_h, padding, rgen, band, width);
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();
    let igen = images_gen(rd);
    let mgen = math_gen(rd);

    let _t_setup = crate::prof::begin();
    let epoch = height_epoch(struct_ver(rd), width_bits, mermaid_zoom_gen(rd));
    let (rebuilt, base, old_layout) = ensure_block_layout(rd, n, epoch);
    if !rebuilt {
        if let Some(ixs) = take_pending_patch_blocks(rd)
        {
            let mut b = base.borrow_mut();
            for i in ixs.into_iter().filter(|&i| i < n) {
                if let Some(meta) = doc.block_meta(i) {
                    let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
                    let h = f32::from(estimate_height(&meta, chars, ascii, theme, avail, line_h));
                    b.set(i, h);
                }
            }
        }
        patch_pending_image_heights(rd, doc, &base, avail, line_h);
    }

    let derived = derived(rd, doc);
    let math_regions = &derived.borrow().math_regions;
    crate::prof::end("wy.bf.setup", _t_setup);

    if rebuilt {
        let prebuilt = struct_ver(rd) == 0
            && mermaid_zoom_gen(rd) == 0
            && rd
                .borrow()
                .prebuild_slot
                .borrow()
                .as_ref()
                .is_some_and(|p| p.len() == n);
        if !(prebuilt && pass1_from_prebuild(rd, doc, &base, theme, avail, line_h)) {
            *rd.borrow_mut().prebuild_slot.borrow_mut() = None;
            pass1_base_layout(
                doc,
                rd,
                n,
                &base,
                old_layout.as_ref(),
                theme,
                avail,
                line_h,
            );
        }
    }
    let caret_line = docutil::row_col(host.doc(), host.caret()).0;
    let _t_plan = crate::prof::begin();
    let _t_fold = crate::prof::begin();
    let plan = fold_plan(rd, doc, base.clone(), epoch, mgen);
    crate::prof::end("wy.bf.plan.fold", _t_fold);
    let _t_rv = crate::prof::begin();
    let reveal = fold_reveal(rd, doc, caret_line, &plan, &base);
    crate::prof::end("wy.bf.plan.reveal", _t_rv);
    let (deltas, delta_sum, delta_pref) = if reveal.is_empty() {
        (
            plan.deltas.clone(),
            plan.delta_sum,
            plan.delta_pref.clone(),
        )
    } else {
        let eff: Vec<(usize, f32)> = plan
            .deltas
            .iter()
            .copied()
            .filter(|(ix, _)| !reveal.zeroed.contains(ix))
            .collect();
        let s: f32 = eff.iter().map(|&(_, v)| v).sum();
        let pref = build_delta_pref(&eff);
        (
            std::rc::Rc::<[(usize, f32)]>::from(eff.into_boxed_slice()),
            s,
            pref,
        )
    };
    crate::prof::end("wy.bf.plan", _t_plan);

    let (i0, mut i1) = find_band_range(&base, &deltas, &delta_pref, band_lo, band_hi, n);

    let shape_key = (width_bits as u64) ^ mermaid_zoom_gen(rd);
    let mut shaped = pass2_shape_band(
        rd,
        host,
        window,
        doc,
        &base,
        theme,
        &base_font,
        width,
        avail,
        i0,
        i1,
        shape_key,
        caret_line,
        &plan,
        &reveal,
    );
    const BAND_COVER_CAP: usize = 16;
    let mut band_cover_steps = 0;
    while i1 < n && band_cover_steps < BAND_COVER_CAP {
        band_cover_steps += 1;
        let (_, i1_next) = find_band_range(&base, &deltas, &delta_pref, band_lo, band_hi, n);
        if i1_next <= i1 {
            break;
        }
        i1 = i1_next;
        shaped = pass2_shape_band(
            rd,
            host,
            window,
            doc,
            &base,
            theme,
            &base_font,
            width,
            avail,
            i0,
            i1,
            shape_key,
            caret_line,
            &plan,
            &reveal,
        );
    }

    let (rows, total) = pass3_assemble_rows(
        &base,
        &deltas,
        delta_sum,
        shaped,
        i0,
        i1,
        &plan,
        math_regions,
        &reveal,
    );
    crate::prof::end("wy.build_frame", _t_bf);
    Frame {
        padding,
        line_h,
        rows,
        base,
        deltas,
        total: px(total),
        band,
        width,
        rgen,
        igen,
        mgen,
        zgen: mermaid_zoom_gen(rd),
    }
}

/// 空文档（0 块）占位帧。
#[allow(clippy::too_many_arguments)]
fn empty_frame(
    rd: &RefCell<WyRenderModel>,
    line_h: Pixels,
    padding: Pixels,
    rgen: u64,
    band: (f32, f32),
    width: Pixels,
) -> Frame {
    Frame {
        padding,
        line_h,
        rows: vec![ShapedRow {
            src_line: 0,
            src_lines: 0..1,
            wrapped: std::rc::Rc::new(Vec::new()),
            has_non_ascii: false,
            sub_heights: vec![line_h],
            byte_map: std::rc::Rc::new(Vec::new()),
            editable: false,
            multi_offsets: Vec::new(),
            code_ranges: Vec::new(),
            lh: line_h,
            x_off: px(0.),
            align_offsets: Vec::new(),
            height: line_h,
            top: px(0.),
            kind: IrKind::Blank,
            quoted: false,
            table: None,
            table_edit: None,
            image: None,
            mm: None,
            extra: RowExtra::default(),
        }],
        base: std::rc::Rc::new(std::cell::RefCell::new(BlockLayout::new(
            vec![f32::from(line_h)],
            vec![0],
        ))),
        deltas: std::rc::Rc::from(vec![]),
        total: line_h,
        band,
        width,
        rgen,
        igen: images_gen(rd),
        mgen: math_gen(rd),
        zgen: mermaid_zoom_gen(rd),
    }
}

/// **D1 滚动预热**：把「当前带 + 滚动方向的下一量子带」内尚未整形的块提前 shape 进
/// [`RowShapeCache`]。
pub fn prewarm_band(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    frame: &Frame,
    window: &mut Window,
    scroll_top: f32,
) {
    settle_giant_blocks(rd, host, window, frame, scroll_top);
    let q = 512.;
    let width_bits = f32::from(frame.width).to_bits();
    let sk = (width_bits as u64) ^ mermaid_zoom_gen(rd);
    let caret_line = docutil::row_col(host.doc(), host.caret()).0;
    let plan = fold_plan_rc(rd);
    let doc = host.doc();
    let avail = (f32::from(frame.width) - f32::from(frame.padding) * 2.).max(1.);
    let (blo, bhi) = frame.band;
    let bi = (blo / q) as i32;
    let (bias, bpf, i0, i1, n) = {
        let rdg = rd.borrow_mut();
        let mut st = rdg.prewarm.borrow_mut();
        let vel = st
            .last_band_idx
            .map(|lbi| bi.abs_diff(lbi) as usize)
            .unwrap_or(1);
        if let Some(lbi) = st.last_band_idx {
            if bi != lbi {
                st.dir = if bi > lbi { 1 } else { -1 };
                st.bias = 0;
            }
        } else {
            st.last_band_idx = Some(bi);
            return;
        }
        st.last_band_idx = Some(bi);
        if st.dir == 0 {
            return;
        }
        if st.idle_skip(frame, scroll_top) {
            return;
        }
        let (tlo, thi) = if st.dir > 0 { (bhi, bhi + q) } else { (blo - q, blo) };
        if tlo < 0. || tlo >= thi {
            return;
        }
        let (i0, i1) = frame.band_block_range(tlo, thi);
        if i0 >= i1 {
            return;
        }
        let bpf = st.effective_budget(vel);
        let n = i1 - i0;
        if st.bias >= n {
            st.bias = 0;
        }
        let bias = st.bias;
        (bias, bpf, i0, i1, n)
    };
    let _t = crate::prof::begin();
    let t0 = std::time::Instant::now();
    let time_budget = std::time::Duration::from_millis(8);
    let mut done = 0usize;
    let mut k = i0 + bias;
    loop {
        if done >= bpf || t0.elapsed() > time_budget {
            break;
        }
        let srcline = frame.base.borrow().src_line(k) as usize;
        let (span_lines, span_bytes) = block_span(doc, &frame.base, k);
        let is_medium_code = doc
            .block_meta(k)
            .map(|m| m.kind == RowKind::Code && span_lines > 8)
            .unwrap_or(false);
        let big = span_lines > GIANT_SPAN_LINES
            || span_bytes > GIANT_SPAN_BYTES as i64
            || is_medium_code;
        let blocked = match &plan {
            Some(p) => {
                p.sets.fold_lines.contains(&srcline)
                    || p.sets.math_body.contains(&srcline)
                    || p.sets.math_start.contains_key(&srcline)
            }
            None => false,
        };
        let cache_miss = {
            let rdg = rd.borrow_mut();
            let mut rc = rdg.shape_rows.borrow_mut();
            rc.get(block_content_id(doc, k), sk).is_none()
        };
        if !blocked && !big && caret_line != srcline && cache_miss {
            if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
                #[cfg(feature = "test-mode")]
                bump_prewarm_shapes(rd);
                // 先 clone 落局部：`&rd.borrow().theme` 直写实参时临时 `Ref` 活到整条调用
                // 语句结束，会跨 image 行 shape 内 `lazy_ensure_image_size` 的
                // `rd.borrow_mut()` → `RefCell already borrowed` panic。
                let theme = rd.borrow().theme.clone();
                let base_font = rd.borrow().theme.font.clone();
                let row = crate::shape::shape_docrow(
                    r,
                    rd,
                    host,
                    window,
                    frame.width,
                    avail,
                    &theme,
                    &base_font,
                );
                if !shaped_row_has_notdef(&row) {
                    rd.borrow_mut()
                        .shape_rows
                        .borrow_mut()
                        .put(block_content_id(doc, k), sk, row);
                }
            }
            done += 1;
        }
        k += 1;
        if k >= i1 {
            k = i0;
        }
        if k == i0 + bias {
            break;
        }
    }
    crate::prof::end("wy.prewarm", _t);
    {
        let rdg = rd.borrow_mut();
        let mut st = rdg.prewarm.borrow_mut();
        st.bias = (bias + bpf) % n;
    }
}

/// I4 巨型代码块「静止权威整形」。
pub fn settle_giant_blocks(
    rd: &RefCell<WyRenderModel>,
    host: &dyn WyRenderHost,
    window: &mut Window,
    frame: &Frame,
    scroll_top: f32,
) {
    let k = {
        let rdg = rd.borrow_mut();
        let mut st = rdg.prewarm.borrow_mut();
        if st.pending_giant.is_empty() {
            st.giant_idle = 0;
            st.giant_sig = None;
            return;
        }
        let sig = PrewarmSig::of(frame, scroll_top);
        if st.giant_sig != Some(sig) {
            st.giant_idle = 0;
            st.giant_sig = Some(sig);
        } else {
            st.giant_idle = st.giant_idle.saturating_add(1);
        }
        if st.giant_idle < crate::prewarm::Prewarm::GIANT_SETTLE_IDLE_FRAMES {
            #[cfg(not(feature = "test-mode"))]
            window.request_animation_frame();
            return;
        }
        let n = host.doc().block_count();
        st.pending_giant.retain(|&k| k < n);
        let (bi0, bi1) = frame.band_block_range(frame.band.0, frame.band.1);
        let Some(pos) = st.pending_giant.iter().position(|&k| k >= bi0 && k < bi1) else {
            st.giant_idle = 0;
            return;
        };
        st.pending_giant.remove(pos)
    };
    let doc = host.doc();
    let sk = (f32::from(frame.width).to_bits() as u64) ^ mermaid_zoom_gen(rd);
    let avail = (f32::from(frame.width) - f32::from(frame.padding) * 2.).max(1.);
    if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
        // 同 prewarm_band：临时 `Ref` 活到整条调用结束，先 clone 落局部让借用当场 drop，
        // 避免撞 `lazy_ensure_image_size` 的 `rd.borrow_mut()`。
        let theme = rd.borrow().theme.clone();
        let base_font = rd.borrow().theme.font.clone();
        let row = crate::shape::shape_docrow(
            r,
            rd,
            host,
            window,
            frame.width,
            avail,
            &theme,
            &base_font,
        );
        let cid = block_content_id(doc, k);
        if let Ok(mut b) = frame.base.try_borrow_mut() {
            b.set(k, f32::from(row.height));
        }
        let rc = rd.borrow_mut().shape_rows.borrow_mut().put(cid, sk, row);
        rd.borrow_mut().prewarm.borrow_mut().pin_giant(cid, rc);
        *rd.borrow_mut().frame.borrow_mut() = None;
        #[cfg(not(feature = "test-mode"))]
        window.request_animation_frame();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(heights: Vec<f32>) -> Rc<RefCell<BlockLayout>> {
        let n = heights.len();
        let src: Vec<u32> = (0..n as u32).collect();
        Rc::new(RefCell::new(BlockLayout::new(heights, src)))
    }

    #[test]
    fn band_mid_spans_visible_blocks() {
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 50., 250., 4), (0, 3));
    }

    #[test]
    fn band_start_at_boundary_includes_start_block() {
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 0., 300., 4), (0, 3));
    }

    #[test]
    fn band_above_all_blocks_starts_at_first() {
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], -100., 50., 4), (0, 1));
    }

    #[test]
    fn band_below_all_blocks_returns_empty_span() {
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 500., 600., 4), (4, 4));
    }

    #[test]
    fn delta_shifts_later_block_tops() {
        let base = blk(vec![100., 100., 100., 100.]);
        let deltas = [(1, 50.)];
        assert_eq!(
            find_band_range(&base, &deltas, &build_delta_pref(&deltas), 250., 260., 4),
            (2, 3)
        );
    }

    #[test]
    fn band_hi_beyond_total_clamps_to_n() {
        let base = blk(vec![100., 100., 100.]);
        let deltas = [(2, 100.)];
        assert_eq!(
            find_band_range(&base, &deltas, &build_delta_pref(&deltas), 0., 1e9, 3),
            (0, 3)
        );
    }

    /// 线性参考：区域表逐条找首个命中（与生产二分前一致）。
    fn linear_containing(regions: &[Range<usize>], line: usize, incl_start: bool) -> Option<usize> {
        regions.iter().position(|r| {
            if incl_start {
                line >= r.start && line < r.end
            } else {
                line > r.start && line < r.end
            }
        })
    }

    /// M3-A3 对拍：`region_containing` 二分 == 线性参考，覆盖空表、空区间、紧邻、
    /// 单行、区域前后/边界/间隙各形态。
    #[test]
    fn region_containing_matches_linear_reference() {
        let regions: Vec<Range<usize>> = vec![
            0..3,
            3..3, // 空区间（不应命中任何行）
            5..6, // 单行
            8..8,
            9..14,
            14..17,
            20..21,
            24..30,
        ];
        assert_eq!(
            super::region_containing(&[], |r: &Range<usize>| r.clone(), 3, true),
            None,
            "空表"
        );
        for incl in [true, false] {
            for line in 0..35usize {
                assert_eq!(
                    super::region_containing(&regions, |r| r.clone(), line, incl),
                    linear_containing(&regions, line, incl),
                    "incl_start={incl} line={line}: 二分 != 线性参考"
                );
            }
        }
    }

    /// M3-A3 对拍：`folded_body_at` 分组二分 == 旧线性 `find` 首中；含跨组重叠
    /// （折叠标题体包住折叠 callout）与 alert 优先语义。
    #[test]
    fn folded_body_at_matches_linear_find_with_cross_group_overlap() {
        // 构造序 = 建表序：alert 折叠体组（前） ‖ 标题折叠体组（后）。
        let bodies: Vec<Range<usize>> = vec![
            10..16, // alert 组 0
            30..60, // alert 组 1（包住标题 40..50）
            70..71, // alert 组 2
            2..20, // 标题组 0（包住 alert 10..16）
            40..50, // 标题组 1
        ];
        let split = 3usize;
        for line in 0..80usize {
            let bin = super::folded_body_at(&bodies, split, line).cloned();
            let lin = bodies
                .iter()
                .find(|r| line > r.start && line < r.end)
                .cloned();
            assert_eq!(bin, lin, "line={line}: 分组二分 {bin:?} != 线性首中 {lin:?}");
        }
        // 关键边界抽查：caret 在标题体内但 callout 外（旧行为=标题体，不能错收成 alert）。
        let b = [10..16, 2..20usize];
        assert_eq!(super::folded_body_at(&b, 1, 3).cloned(), Some(2..20));
        assert_eq!(super::folded_body_at(&b, 1, 16).cloned(), Some(2..20));
        assert_eq!(super::folded_body_at(&b, 1, 2).cloned(), None, "标题行本身不折叠");
        assert_eq!(super::folded_body_at(&b, 1, 20).cloned(), None);
    }
}
