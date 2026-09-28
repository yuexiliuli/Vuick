//! WYSIWYG **帧装配层（frame）**：把已 shape 的行按可视带组装成 [`super::element::Frame`]
//! （带外块按 [`estimate_height`] 占位）；含 notdef 自愈的「回退字体行」识别与诊断
//! （`row_uses_fallback` / `debug_special_row`）、**D1 滚动预热**（`prewarm_band`，滚动穿越
//! 时命中 `RowShapeCache` 摊平 shape 突发）与 emoji 冷启动预热包装。
//!
//! 依赖方向：`types ← math ← shape ← frame ← element(胶水)`；经 `mod.rs` 的
//! `use super::*` 直取 [`super::element`] 布局类型、[`super::shape`] 整形与
//! [`super::math`]/[`super::geom`]，并访问根级 [`super::WyEditor`]/[`super::RowShapeCache`]。
//!
//! ⚠️ 本模块不整块重导出到 `mod.rs`（内部 `warm_emoji_rasterizer` 与根级同名函数冲突）；
//! `element.rs` 胶水用 `use super::frame::{build_frame, prewarm_band, warm_emoji_rasterizer}`。

use super::*;

use gpui::{Pixels, Window};

use crate::markdown::doc::DocRow;
use crate::markdown::ir::IrKind;
use crate::markdown::parse::RowKind;
use crate::markdown::parse::inline_style::MathRegion;

/// 冷启动首次 shape 回退字形（❗⚠✅ 等）时 DWrite 可能给「错误回退字体的非零占位字形」
/// （glyph id≠0，逃过 `.notdef` 检测）→ 渲染方框且会被直接缓存（caret 行正常 / 非
/// caret 行方框的翻转就是它）。用它识别这些行 → 先校验重排再入缓存。纯 CJK（全 Some）、
/// 测试 mock（全 None，其 layout 恒 FontId(0)）与纯 emoji 行不触发。
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

/// 诊断：`WY_NOTDEF_DEBUG=1` 时输出含非 ASCII 字形的行的真实整形结果（每 run 的
/// 字体名/font_id/is_emoji/glyph id）。只在「字形签名变化」时打印一次，配合点击复现
/// 「caret 行正常 / 非 caret 行方框」的翻转——方框行与正常行的 run 字体/glyph 差异会
/// 直接暴露（例如冷启动错误回退字体 vs 正确的 Segoe UI Emoji，glyph id 都非 0）。
static LAST_SPECIAL_ROW: std::sync::Mutex<Option<(usize, String)>> =
    std::sync::Mutex::new(None);
fn debug_special_row(
    editor: &WyEditor,
    window: &Window,
    rl: usize,
    srow: &ShapedRow,
    base_font: &gpui::Font,
) {
    if std::env::var_os("WY_NOTDEF_DEBUG").is_none() {
        return;
    }
    let txt = editor.doc().line_str(rl);
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


/// **Pass 1**（仅结构性变更时）：全块**基础高**（与折叠/卡片态无关）+ 起始源码行。
///
/// **简要**：先把整篇每个段落的高度**粗算**出来（不排版，只按内容量估），存进 `base`。
/// 一次编辑只影响一小块 → 前面没动的抄旧高、中间重算、后面再抄旧高，避免大文档每次
/// 敲一下键盘却把全文高度重扫一遍（旧实现 183–375ms → 现在只重估中段）。
///
/// 三段式增量：结构编辑后块数组 = [未变前缀][变化中段][未变后缀]——编辑线之前的块
/// 前缀高度沿用；编辑影响区间的中段重新估算；之后尾部块内容未变 → 高度直接沿用旧
/// 布局（只平移 src_line）。结构编辑从「dirty_line → 文尾」全后缀重估（大文档
/// Enter/代码/表格 183-375ms）降到「仅中段估算 + Vec 复制」。
#[allow(clippy::too_many_arguments)] // 行布局输入互为正交
fn pass1_base_layout(
    doc: &Doc,
    editor: &WyEditor,
    n: usize,
    base: &Rc<RefCell<BlockLayout>>,
    old_layout: Option<&Rc<RefCell<BlockLayout>>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) {
    let _t_p1 = crate::prof::begin();
    let dirty_line = editor.struct_dirty_line();
    let old_end_line = editor.old_edit_end_line();
    let line_delta = editor.edit_line_delta();
    let doc_lines = doc.line_starts().len();
    // 前缀复用长度：块 k 完全落在 dirty_line 之上（块末行 = 下块首行-1 < dirty）
    // → 内容不变、索引稳定。src_line 随块下标单调 → **二分**求「src_line ≤
    // dirty_line 的块数 p」，前缀 keep = p.saturating_sub(1)（编辑线所在块之前）。
    // 旧实现从 0 逐块线性扫（大文档每次 Pass1 O(块数) ~2-5ms）。
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
    // 二分界内的块 0..=keep 下标稳定（内容/索引未变）→ 前缀高度沿用。
    let keep = p.saturating_sub(1);
    let old_holder = old_layout.cloned(); // 保活旧布局 Rc 以便借用
    let old = old_holder.as_ref().map(|rc| rc.borrow());
    let (old_heights, old_src): (Option<&[f32]>, Option<&[u32]>) = match &old {
        Some(ob) => (Some(ob.heights()), Some(ob.src_lines())),
        None => (None, None),
    };
    let (mut heights, mut src_lines) = match &old {
        Some(ob) => {
            let k = keep.min(ob.n());
            (ob.heights()[..k].to_vec(), ob.src_lines()[..k].to_vec())
        }
        None => (Vec::with_capacity(n), Vec::with_capacity(n)),
    };
    // 旧尾部起点 j：首个旧块 src_line > 编辑影响区间终点。
    let old_exist = old_src.is_some();
    let j = match old_src {
        Some(os) => os.partition_point(|&s| (s as usize) <= old_end_line),
        None => 0,
    };
    // 新尾部起点 k：首个新块 src_line ≥ 旧尾部 src_line + 行数净增。旧布局缺失
    // （首建）或编辑近文末（无旧尾部）时中段扫到底。块 src_line 随下标单调 →
    // 直接**二分**首个 `src_line ≥ tail_src`，替代旧实现对 `[keep, n)` 全部块逐个
    // `doc.block_meta` + `collect` 成 Vec（中段编辑 57k 块 × O(log) 是 pass1 的
    // 4-6ms 主成本）。
    let tail_src = match (old_src, old_src.and_then(|os| os.get(j).copied())) {
        (_, Some(s)) => s as i64 + line_delta,
        _ => doc_lines as i64,
    };
    let k = if !old_exist {
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
    // 中段（编辑所在 → 新尾部起点）重新估算。
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
            // A2: ```mermaid 围栏 → 图卡片高（与 shape_code_row 同式）。
            let mut src = String::new();
            for k in 0..meta.aux {
                if k > 0 {
                    src.push('\n');
                }
                src.push_str(&doc.line_str(srcline + 1 + k as usize));
            }
            f32::from(mermaid_card_height(&src)) * editor.mermaid_zoom(srcline)
        } else if meta.kind == RowKind::Image {
            // 图片行：用当前加载尺寸按 `shape_image_row` 同式算高（占位→真实由
            // `igen` 触发整表重建，带内外必须同一公式防滚动跳）。
            let src = crate::markdown::parse::parse_image_line(doc.line_str(srcline).trim())
                .map(|(_, s)| s)
                .unwrap_or_default();
            f32::from(image_row_height(editor, &src, avail))
        } else {
            let (chars, ascii) = doc.line_chars_ascii(meta.src_line as usize);
            f32::from(estimate_height(&meta, chars, ascii, theme, avail, line_h))
        };
        (h, meta.src_line)
    };
    for i in heights.len()..k {
        let (h, sl) = est_block(i);
        heights.push(h);
        src_lines.push(sl);
    }
    // 尾部复用：旧 [j..] 高度原样搬入新 [k..]（长度恰好吻合时才成立）。
    let mut tail_reused = false;
    if let (Some(oh), Some(os)) = (old_heights, old_src)
        && k <= n && j <= os.len() && os.len() - j == n - k {
            src_lines.extend(os[j..].iter().map(|&s| (s as i64 + line_delta).max(0) as u32));
            heights.extend_from_slice(&oh[j..]);
            tail_reused = true;
        }
    // 兜底：尾部未复用（首建 / 对齐异常）→ 剩余块照旧逐块重估（正确 > 快）。
    if !tail_reused {
        for i in heights.len()..n {
            let (h, sl) = est_block(i);
            heights.push(h);
            src_lines.push(sl);
        }
    }
    debug_assert_eq!(heights.len(), n, "Pass1 三段式产出的块数与新索引一致");
    base.borrow_mut().rebuild(heights, src_lines, doc_lines);
    crate::prof::end("wy.pass1", _t_p1);
}

/// 在可视带内二分定位 [i0, i1) 块区间。
///
/// **简要**：用每段的「有效累计高度」做二分，找出当前这一屏框住的是哪些段落块
/// （返回下标区间 [i0, i1)）——后面只在屏内这几块上做昂贵的事，屏外全用粗算高度占位。
///
/// `eff_top(i)` = 基础前缀 + 动态增量前缀；单调 → 二分 `count_true` 得区间。
fn find_band_range(
    base: &Rc<RefCell<BlockLayout>>,
    deltas: &[(usize, f32)],
    band_lo: f32,
    band_hi: f32,
    n: usize,
) -> (usize, usize) {
    let _t_bis = crate::prof::begin();
    // `deltas` 前缀和表：`delta_pref[p]` = 前 p 项增量之和（p ∈ [0, len]），一次
    // O(len) 建成 → 任意 `eff_top(i)` 只需 O(log len) 定位（partition_point 找
    // 首个 `ix >= i`），旧实现每个二分步都对前缀现算 O(len) 求和：带内 3000 条增量
    // 时 bisect ~2.8ms/帧、Pass3 逐行 O(rows×len) ~27ms/帧。
    let delta_pref: Vec<f32> = {
        let mut v = Vec::with_capacity(deltas.len() + 1);
        let mut s = 0f32;
        v.push(0.);
        for &(_, d) in deltas.iter() {
            s += d;
            v.push(s);
        }
        v
    };
    let eff_top = |i: usize| -> f32 {
        let b = base.borrow();
        let k = deltas.partition_point(|&(ix, _)| ix < i);
        b.top(i) + delta_pref[k]
    };
    // 二分终止右闭（k ∈ [0, n]）：返回有效前缀满足谓词的 k 计数。
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
///
/// **简要**：只有**看得见的这一屏**才请平台真的排一遍版（量出真实高度、算好每个字的
/// 形状）。排好的块进缓存，滚动回来直接复用；首排成豆腐块（回退字形没起来）的块
/// 隔一帧用新鲜排版重试一次，直到干净字形出来——这就是「滚动延后上色 / 自愈」。
///
/// reveal 覆盖：caret 所在区域的行不再 blocked（还原源码 shape）。
/// 行整形缓存（滚动/带重建复用）：键 = epoch ^ mgen；caret 严格落在含行内公式的
/// 文本行时（reveal，shape 依赖光标）该行不缓存；编辑作废由 `replace_range` 负责。
#[allow(clippy::too_many_arguments)] // 输入互为正交的行整形上下文
fn pass2_shape_band(
    editor: &WyEditor,
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
    let fold_lines = &plan.fold_lines;
    let math_body = &plan.math_body;
    let math_start = &plan.math_start;
    let mut shaped: Vec<(usize, ShapedRow)> = Vec::new();
    let mut i = i0;
        while i < i1 {
            let srcline = base.borrow().src_lines()[i] as usize;
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
                let s = base.borrow().src_lines()[i] as usize;
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
            // A1「命中先行」+ 只物化 miss/caret 块（连续段合并批量 parse）。
            let (need_row, mut hit_map) =
                band_hit_test(editor, base, lo, hi, caret_line, shape_key);
            let mut rows_map = band_parse_rows(doc, &need_row);
            // 整带顺序装配（Pass3 依赖 `shaped` 与块序一致）：命中 → 浅克隆
            // （heavy 数据都是 Arc/Rc，top 在 Pass3 现设）；否则从物化行 shape。
            let _t_shape = crate::prof::begin();
            for k in lo..hi {
                let srow = if let Some(rc) = hit_map.remove(&k) {
                    (*rc).clone()
                } else if let Some(last) = editor.cooling_row(k) {
                    // 冷却帧：不调用 gpui shape（被钉的 `LineLayoutCache` 键本帧不被
                    // 引用 → 帧末自然淘汰），复用上一份 notdef 行几何继续显示。次帧
                    // 重排才是新键、真打到平台（DWrite 回退自愈的必要条件）。
                    // 强制下一帧：静态视图不会再自渲染，若不请求重绘则自愈停摆。
                    window.refresh();
                    #[cfg(test)]
                    editor.bump_coolings();
                    (*last).clone()
                } else {
                    let r = rows_map.remove(&k).expect("未命中 / caret 行应已被物化");
                    let is_caret = !matches!(r.kind, IrKind::Code | IrKind::Table | IrKind::Image)
                        && caret_line == r.src_line;
                    let rl = r.src_line;
                    #[cfg(test)]
                    editor.bump_reshapes();
                    let srow = shape_docrow(r, editor, window, width, avail, theme, base_font);
                    if shaped_row_has_notdef(&srow) {
                        // —— 冷启动回退未命中（glyph id=0）→「冷却一帧 → 新鲜平台重排」——
                        // 一律重试（**不**因“回退已温”就静默缓存成豆腐块：探针只覆盖少数
                        // (font,size)，真实行字号/字体可能未探到而首排仍坏——实测首排坏、
                        // 次排好；冷却一帧让被钉的 LineLayoutCache 键淘汰，次排才是新键、
                        // 真打到 DWrite 平台）。
                        if editor.notdef_settled(k) {
                            // 已判定缺字形（仅 caret 行会到此处，非 caret 行在 settled 时
                            // 已入缓存、走命中路径）：直接显示，不再冷却空转。
                            srow
                        } else if editor.cool_start(k) {
                            // 连续 N 次真实平台重排仍 .notdef → 判定字符确无字形
                            // （tofu 即正确显示），从此不再重排空转。
                            notdef_debug(format_args!(
                                "pass2 give-up k={} src={} caret={is_caret}",
                                k, rl
                            ));
                            if is_caret {
                                srow
                            } else {
                                let rc = editor.shape_rows.borrow_mut().put(k, shape_key, srow);
                                (*rc).clone()
                            }
                        } else {
                            // 入冷却：本帧几何留作下帧显示、且下帧不 shape（被钉的
                            // LineLayoutCache 键不被引用 → 帧末淘汰 → 再下帧重排才是
                            // 新键、真重新打到 DWrite）。caret 行同样受益（否则首行 ❗
                            // 每帧命中旧坏键，豆腐块永不消失）。强制下一帧推进自愈。
                            window.refresh();
                            editor.cool_keep_last(k, std::rc::Rc::new(srow.clone()));
                            notdef_debug(format_args!(
                                "pass2 notdef k={} src={} caret={is_caret} -> cooling",
                                k, rl
                            ));
                            srow
                        }
                    } else if row_uses_fallback(window.text_system(), base_font, &srow)
                        && !editor.fallback_verified(k)
                    {
                        // —— 冷启动回退占位字形（glyph id≠0，检测不到 .notdef）——
                        // 首次遇到用到回退字体（字体族 ≠ 基字体）的行，DWrite 首次 shape
                        // 可能给「错误回退字体里的非零占位字形」→ 渲染成方框，且因 glyph
                        // id≠0 逃过上面的 notdef 检测、会被直接缓存钉住（caret 行正常 /
                        // 非 caret 行方框的翻转就是它）。不缓存首次结果：标记已校验 + 冷却
                        // 一帧强制第二次真实重排（实测第二次即正常），次排走正常缓存路径。
                        editor.mark_fallback_verified(k);
                        editor.verify_cool(k);
                        window.refresh();
                        editor.cool_keep_last(k, std::rc::Rc::new(srow.clone()));
                        notdef_debug(format_args!(
                            "pass2 fallback-verify k={} src={} caret={is_caret}",
                            k, rl
                        ));
                        srow
                    } else {
                        // 干净排（无回退，或已校验过回退字形）：清掉本块此前任何重试/
                        // settled 状态。caret 行若**无行内数学**（reveal 不依赖光标）也入
                        // 缓存——用新鲜的正确答案覆盖早期冷启动缓存的坏行：用户点击该行
                        // 一次后永久修复（否则每次点击离开又回到旧坏行 = “点击翻转”）。
                        editor.clear_notdef(k);
                        if is_caret && !srow.extra.inline_maths.is_empty() {
                            srow // reveal 依赖光标 → 保持不缓存
                        } else {
                            let rc = editor.shape_rows.borrow_mut().put(k, shape_key, srow);
                            (*rc).clone()
                        }
                    }
                };
                let srcl = base.borrow().src_lines()[k] as usize;
                debug_special_row(editor, window, srcl, &srow, base_font);
                base.borrow_mut().set(k, f32::from(srow.height));
                shaped.push((k, srow));
            }
            crate::prof::end("wy.bf.shape", _t_shape);
        }
    crate::prof::end("wy.pass2", _t_p2);
    shaped
}

/// A1「命中先行」：整带内判哪些块**不需要**物化 DocRow——命中 = 缓存键匹配且非 caret 行
/// （caret 行 shape 依赖光标，即便已在缓存也必须强制物化重 shape、不缓存；逐帧至多一行）。
/// 只对「未命中 ∪ caret 行」批量物化（连续段合并复用批量 parse）。本函数内部临时借用
/// `shape_rows` 缓存做命中判定，返回 (need_row, hit_map)。
#[allow(clippy::type_complexity)]
fn band_hit_test(
    editor: &WyEditor,
    base: &Rc<RefCell<BlockLayout>>,
    lo: usize,
    hi: usize,
    caret_line: usize,
    shape_key: u64,
) -> (Vec<usize>, std::collections::HashMap<usize, std::rc::Rc<ShapedRow>>) {
    let mut row_cache = editor.shape_rows.borrow_mut();
    let mut need_row: Vec<usize> = Vec::new();
    let mut hit_map: std::collections::HashMap<usize, std::rc::Rc<ShapedRow>> =
        std::collections::HashMap::new();
    for k in lo..hi {
        let srcline = base.borrow().src_lines()[k] as usize;
        if caret_line == srcline {
            need_row.push(k);
        } else if let Some(rc) = row_cache.get(k, shape_key) {
            if shaped_row_has_notdef(&rc) && !editor.notdef_settled(k) {
                // 命中但含 .notdef（此前冷启动坏字形被缓存/预热写入）：视作 miss，强制以
                // 新鲜平台重排——否则豆腐块被旧缓存钉住整次运行。已 `settled`（判定确无
                // 字形）的块例外：tofu 即正确显示，正常命中。
                need_row.push(k);
            } else {
                hit_map.insert(k, rc);
            }
        } else {
            // 缓存 miss。若存在**旧键**条目（内容/图片/数学/宽度换代把旧行逐出），旧
            // `settled/verified/attempts` 判决对应的是上一代结果——可能在图片解码卡顿窗口
            // 里留下（.notdef 预算烧光→settled / 非零占位字形→verified），换代重排前作废，
            // 让本块按新代重新校验/重试。图片加载完成的 `igen` 换代重建会走到这里 → 自愈。
            if row_cache.key_mismatch(k, shape_key) {
                editor.clear_stale_verdicts(k);
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

/// **Pass 3**：组装带内行（有效 top = 基础前缀 + 动态增量前缀，`set` 后已是最终高度）。
///
/// **简要**：把「粗算高度 + Pass2 排好的真实行」合成这一帧：给每行定屏幕坐标，折叠 /
/// 数学主体的 0 高行直接跳过，数学块换成卡片，普通块取排好的行贴上坐标，最后算出
/// 整篇总高（滚动条量程用）。
///
/// 折叠 body / 数学 body 0 高不物化；被 reveal 的源码行直接出整形结果，否则数学块
/// 挂卡片、普通块浅克隆 `shaped` 行并现设 top。
#[allow(clippy::too_many_arguments)] // 输入互为正交的行组装上下文
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
    let math_start = &plan.math_start;
    let math_body = &plan.math_body;
    let fold_lines = &plan.fold_lines;
    let mut rows: Vec<ShapedRow> = Vec::new();
    let mut shaped_iter = shaped.into_iter().peekable();
    let total = {
        let b = base.borrow();
        b.total()
    } + delta_sum;
    // 各路行按 top 从 base+deltas 现算；total 已含全部。
    // `deltas` 按下标升序 → 用滚动指针 + 累加器：每行 O(1) 摊还拿到「ix<i 的增量和」
    // 与「ix==i 的命中增量」，替代旧实现的逐行 `partition_point`+O(k) 求和+线性
    // `find`（带 3000 条增量时 Pass3 ~27ms/帧 → ~2ms）。
    let mut di = 0usize;
    let mut acc = 0f32;
    for i in i0..i1 {
        while di < deltas.len() && deltas[di].0 < i {
            acc += deltas[di].1;
            di += 1;
        }
        let srcline = base.borrow().src_lines()[i] as usize;
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
            continue; // 折叠 body / 数学 body：0 高不物化
        }
        let shown_source = reveal.contains(srcline); // caret 在此区域 → 显示源码而非卡片/折叠
        let folded_here = !shown_source
            && (fold_lines.contains(&srcline) || math_body.contains(&srcline));
        if !folded_here
            && !shown_source
            && let Some(&ridx) = math_start.get(&srcline)
        {
            // 数学块卡片（A1）。
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

/// 构建一帧：**窗口化**——仅 `band`（元素局部 y）内的块被物化 + shape，
/// 带外块用 [`estimate_height`] 占位（空 `wrapped`、空 `byte_map`），块高循环
/// 仍覆盖全篇以得到正确 total（滚动条量程）与行序。
pub(crate) fn build_frame(
    editor: &WyEditor,
    window: &mut Window,
    width: Pixels,
    band: (f32, f32),
) -> Frame {
    let _t_bf = crate::prof::begin();
    // 字形/字号一律取**主题**（创建时定格），不读 `window.text_style()`——measure 与
    // prepaint 两阶段该值不同（栈内带/不带 `live-body.text_size`），每帧现取会在两种
    // 字号间交替，滚动时文字忽大忽小。
    let theme = &editor.theme;
    let line_h = theme.para_line();
    let base_font = theme.font.clone();
    let doc = editor.doc();
    let n = doc.block_count();
    let padding = px(12.);
    let rgen = editor.content_gen();

    if n == 0 {
        return Frame {
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
                height: line_h,
                top: px(0.),
                kind: IrKind::Blank,
                table: None,
                table_edit: None,
                image: None,
                mm: None,
                extra: RowExtra::default(),
            }],
            base: std::rc::Rc::new(std::cell::RefCell::new(BlockLayout::new(
                vec![f32::from(line_h)],
                vec![0],
                doc.line_starts().len(),
            ))),
            deltas: std::rc::Rc::from(vec![]),
            total: line_h,
            band,
            width,
            rgen,
            igen: editor.images_gen(),
            mgen: editor.math_gen(),
            zgen: editor.mermaid_zoom_gen(),
        };
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();
    let igen = editor.images_gen();
    let mgen = editor.math_gen();

    // 基础布局：代 = `(struct_ver, 图片/数学/宽度/mermaid缩放)`。滚动/带移动与**非结构性
    // 编辑**都复用（编辑只对受影响块做 Fenwick 点更新），不再每档/每键对整个 O(n) 高度
    // 数组重算前缀和（旧实现在内容代变化时对全篇重扫 ~400ms，是「一编辑就卡」之一）。
    let _t_setup = crate::prof::begin();
    let epoch = super::height_epoch(editor.struct_ver(), igen, width_bits, editor.mermaid_zoom_gen());
    let (rebuilt, base, old_layout) = editor.ensure_block_layout(n, epoch);
    // 非结构性编辑：基础布局复用，只修补受影响块高（估计 → Fenwick 点更新，O(log n)）。
    if !rebuilt
        && let Some(ixs) = editor.take_pending_patch_blocks()
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

    // 结构扫描（derived 缓存 → O(1) Rc 复用）；`math_regions` 在 Pass3 卡片组装用。
    let derived = editor.derived();
    let math_regions = &derived.borrow().math_regions;
    crate::prof::end("wy.bf.setup", _t_setup);

    // —— 一帧流水线（简要）——
    // ① Pass1：全篇先粗算每一段的高度（仅结构变化时跑）；
    // ② 折叠 / caret 展开计划（算出 deltas，跨带复用）；
    // ③ 二分出当前这屏框住的块区间 [i0, i1)；
    // ④ Pass2：只对这一屏真排版（排过的入缓存）；
    // ⑤ Pass3：合成坐标、出帧。
    // 滚动 / 普通打字只重做 ③④⑤，①②的结果跨带复用（廉价帧）。

    if rebuilt {
        pass1_base_layout(
            doc,
            editor,
            n,
            &base,
            old_layout.as_ref(),
            theme,
            avail,
            line_h,
        );
    }
    // 折叠/重排计划（与可视带无关、不含 caret，跨带/跨光标复用——滚动与输入不再重算
    // `math_start`/`fold_lines`/`deltas`）。caret 所在区域的 reveal 由 `fold_reveal`
    // 按带现算叠加（最多一个数学块 + 一个折叠体）。⚠️ 必须在 Pass1 之后调用：
    // `deltas` 读基础布局当前高度（重建过的 epoch 使缓存键变化 → 自动重建）。
    let caret_line = editor.row_col(editor.caret()).0;
    let _t_plan = crate::prof::begin();
    let _t_fold = crate::prof::begin();
    let plan = editor.fold_plan(base.clone(), epoch, mgen);
    crate::prof::end("wy.bf.plan.fold", _t_fold);
    let _t_rv = crate::prof::begin();
    let reveal = editor.fold_reveal(caret_line, &plan, &base);
    crate::prof::end("wy.bf.plan.reveal", _t_rv);
    // 有效增量：无 reveal（常见：光标在普通行）→ 直接复用计划 deltas（零拷贝）；
    // 有 reveal → 过滤掉被归零的块（最多几个），每带一次 O(k) 拷贝。
    let (deltas, delta_sum): (std::rc::Rc<[(usize, f32)]>, f32) = if reveal.is_empty() {
        (plan.deltas.clone(), plan.delta_sum)
    } else {
        let eff: Vec<(usize, f32)> = plan
            .deltas
            .iter()
            .copied()
            .filter(|(ix, _)| !reveal.zeroed.contains(ix))
            .collect();
        let s: f32 = eff.iter().map(|&(_, v)| v).sum();
        (eff.into_boxed_slice().into(), s)
    };
    crate::prof::end("wy.bf.plan", _t_plan);

    // 带内块区间 [i0, i1)：用「有效前缀」（基础 + 动态增量，单调）二分。
    let (i0, i1) = find_band_range(&base, &deltas, band_lo, band_hi, n);

    // Pass 2：带内非折叠/非数学块 shape，真实高 `set` 回基表（带外沿用收敛真值）。
    // reveal 覆盖：caret 所在区域的行不再 blocked（还原源码 shape）。
    // 行整形缓存（滚动/带重建复用）：键 = epoch ^ mgen；caret 严格落在含行内公式的
    // 文本行时（reveal，shape 依赖光标）该行不缓存；编辑作废由 `replace_range` 负责。
    let shape_key = (epoch ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)) ^ editor.code_parse_gen();
    let shaped = pass2_shape_band(
        editor,
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

    // Pass 3：组装带内行（有效 top = 基础前缀 + 动态增量前缀，`set` 后已是最终高度）。
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
        zgen: editor.mermaid_zoom_gen(),
    }
}


/// **D1 滚动预热**：把「当前带 + 滚动方向的下一量子带」内尚未整形的块提前 shape 进
/// [`RowShapeCache`]（键 = `epoch ^ mgen`，与 band/帧键**无关**，与 `build_frame` Pass2
/// 命中判据同式）→ 带穿越时 Pass2 直接命中缓存，每 512px 一次的 ~6.4ms/band shape
/// 突发被摊平（预算按滚动速度自适应：静止 3 块/帧、快滚最高 24 块/帧，见
/// [`super::Prewarm::effective_budget`]）。
///
/// 硬约束：
/// - **只 `put` 缓存、绝不 `base.set()`**——不提前改 total，滚动条量程/布局零抖动；
///   真高收敛仍由进带时 Pass2 的 `set` 完成。
/// - 跳过折叠/数学块（不自走整形缓存）与 caret 行（shape 依赖光标，进带时 Pass2 强制
///   重 shape）；fold_plan 无碍正确性——Pass2 的 `blocked` 判定始终以现算 plan 为准，
///   预热条目若因 reveal 变活跃也只是浅克隆复用。
/// - 方向只由带序号跨界（`band.top/512` 取整变化）判定；预算按速度自适应
///   （`Prewarm::effective_budget`：基础 `blocks_per_frame` + 速度×4，硬上限 24），
///   `bias` 滑动窗每帧推进、回绕兜底——区间扫满即全部命中/跳过，空转开销≈HashMap 查找。
pub(crate) fn prewarm_band(e: &WyEditor, frame: &Frame, window: &mut Window) {
    let q = 512.;
    let mut st = e.prewarm.borrow_mut();
    let (blo, bhi) = frame.band;
    let bi = (blo / q) as i32;
    // 滚动速度（带/帧）：本帧相对上一帧登记的带序号跳跃量。快滚（跳多档）时按速度放大
    // 每帧预热预算，否则 3 块/帧追不上视口 → 落点带仍冷、每带穿越补 Pass2 突发。
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
        // 首帧：无方向可判，只登记带序号。
        st.last_band_idx = Some(bi);
        return;
    }
    st.last_band_idx = Some(bi);
    if st.dir == 0 {
        return;
    }
    // 目标量子带 = 当前带沿方向平移一个量子。
    let (tlo, thi) = if st.dir > 0 { (bhi, bhi + q) } else { (blo - q, blo) };
    if tlo < 0. || tlo >= thi {
        return;
    }
    let (i0, i1) = frame.band_block_range(tlo, thi);
    if i0 >= i1 {
        return;
    }
    // shape_key 与 `build_frame` 完全同式（epoch = struct_ver ^ igen ^ width_bits）。
    let width_bits = f32::from(frame.width).to_bits();
    let epoch = super::height_epoch(e.struct_ver(), e.images_gen(), width_bits, e.mermaid_zoom_gen());
    let sk = (epoch ^ e.math_gen().wrapping_mul(0xC2B2_AE3D_27D4_EB4F)) ^ e.code_parse_gen();
    let caret_line = e.row_col(e.caret()).0;
    let plan = e.fold_plan_rc();
    let doc = e.doc();
    let avail = (f32::from(frame.width) - f32::from(frame.padding) * 2.).max(1.);
    let bpf = st.effective_budget(vel);
    let mut rc = e.shape_rows.borrow_mut();
    let n = i1 - i0;
    if st.bias >= n {
        st.bias = 0;
    }
    let _t = crate::prof::begin();
    // 每帧预热**时间硬上限**：块数预算（最高 24 块/帧）× 昂贵块（debug 单块可就几十
    // ms）能把单帧干到 1.3s（实测快滚跨新带）。按已过时间截断，再不阻塞主线程——
    // 预热只是「提前整形、进带可浅克隆复用」，少预几张由进带 Pass2 补，不影响正确性。
    let t0 = std::time::Instant::now();
    let time_budget = std::time::Duration::from_millis(8);
    let mut done = 0usize;
    let mut k = i0 + st.bias;
    loop {
        if done >= bpf || t0.elapsed() > time_budget {
            break;
        }
        let srcline = frame.base.borrow().src_lines()[k] as usize;
        // 大块跳过预热：单块 DocRow 覆盖整块源区间，`rows_in_block_window(k,k)` 会
        // 一次性物化并整形整个块（debug 胖表格/长代码段可达 1.6s/块，实测快滚跨界时
        // 单帧窒息）。预热只挑小/中块；大块进带时由 Pass2 权威整形（那时本来就要渲染）。
        // 判定用「字节跨度 + 行跨度」双阈值——行数不反映行的宽度（胖表格多列），字节更准。
        let big = {
            let base_ref = frame.base.borrow();
            let sl = base_ref.src_lines();
            let s0 = sl.get(k).copied().unwrap_or(0) as usize;
            let s1 = sl.get(k + 1).copied().map(|v| v as usize).unwrap_or(s0 + 1);
            let span_lines = s1.saturating_sub(s0);
            let span_bytes = doc
                .block_meta(k)
                .and_then(|m| doc.block_meta(k + 1).map(|n| n.byte as i64 - m.byte as i64))
                .unwrap_or(0);
            // 中大型代码块（≥8 行）也按「大块」跳过预热：syntect 整块解析是 debug 下
            // 的主要单块成本（实测 14 行 rust 块 ~300ms），预热 8ms 预算拦不住它——
            // 交进带时 Pass2 权威整形（那时本来要渲染），滚动帧不被投机整形拖垮。
            let is_medium_code = doc
                .block_meta(k)
                .map(|m| m.kind == RowKind::Code && s1.saturating_sub(s0) > 8)
                .unwrap_or(false);
            span_lines > 64 || span_bytes > 4096 || is_medium_code
        };
        // 与 Pass2 `blocked` 同判据：折叠/math 块不物化整形行。
        let blocked = match &plan {
            Some(p) => {
                p.fold_lines.contains(&srcline)
                    || p.math_body.contains(&srcline)
                    || p.math_start.contains_key(&srcline)
            }
            None => false,
        };
        if !blocked && !big && caret_line != srcline && rc.get(k, sk).is_none() {
            // 与 Pass2 miss 分支同式：整形成功即入缓存（随后 Pass2 命中时浅克隆复用）。
            // 含 .notdef 的坏行**不入缓存**——进带后由 Pass2 以「冷却一帧 → 新鲜重排」
            // 自愈（一律重试，不为“回退已温”静默缓存）。
            if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
                #[cfg(test)]
                e.bump_prewarm_shapes();
                let row =
                    shape_docrow(r, e, window, frame.width, avail, &e.theme, &e.theme.font);
                if !shaped_row_has_notdef(&row) {
                    rc.put(k, sk, row);
                }
            }
            done += 1;
        }
        k += 1;
        if k >= i1 {
            k = i0;
        }
        if k == i0 + st.bias {
            break; // 一圈完成（全命中/跳过）→ 本帧收工，防止空转
        }
    }
    crate::prof::end("wy.prewarm", _t);
    st.bias = (st.bias + bpf) % n;
}

/// Windows 冷启动 color-emoji 光栅预热（一次/进程）。实测：DWrite 在冷进程里**首次**
/// 光栅化彩色 COLR 字形时 `rasterize_color` 失败，gpui 回退为单色（彩色 emoji 没有
/// 单色轮廓 → 空白/方框），且 `sprite_atlas` 会按 (font,glyph,size) 把这份坏结果钉住
/// 整次运行。这里在 WYSIWYG 首帧 paint 阶段（任何真实行打印之前）打热彩字管线；
/// 具体实现见 [`super::warm_emoji_rasterizer`]（共享给预览/源码编辑区，先画的窗格先
/// 预热，杜绝被先画窗格毒化）。
pub(crate) fn warm_emoji_rasterizer(window: &mut Window) {
    if !super::warm_emoji_is_done() {
        super::warm_emoji_rasterizer(window, "wysiwyg");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用「等高块 + src_line = 块下标」构造合成布局（与 `build_frame` 首建同构）。
    fn blk(heights: Vec<f32>) -> Rc<RefCell<BlockLayout>> {
        let n = heights.len();
        let src: Vec<u32> = (0..n as u32).collect();
        Rc::new(RefCell::new(BlockLayout::new(heights, src, n)))
    }

    #[test]
    fn band_mid_spans_visible_blocks() {
        // 4×100px 块，band 50..250 → 覆盖块 0/1/2（块 3 top=300 在带外）。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], 50., 250., 4), (0, 3));
    }

    #[test]
    fn band_start_at_boundary_includes_start_block() {
        // band 0..300：块 3 top=300 不含（< band_hi 严格），块 0 因 top<=0 计入。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], 0., 300., 4), (0, 3));
    }

    #[test]
    fn band_above_all_blocks_starts_at_first() {
        // band 顶部在首块内（负区），i0 饱和到 0；只含块 0。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], -100., 50., 4), (0, 1));
    }

    #[test]
    fn band_below_all_blocks_returns_empty_span() {
        // band 在整个文档之下（滚到底）→ 空区间 (4, 4)。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], 500., 600., 4), (4, 4));
    }

    #[test]
    fn delta_shifts_later_block_tops() {
        // 块 1 增量 +50 → 块 2 有效 top = 200+50 = 250 …band 250..260 恰好罩住块 2。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[(1, 50.)], 250., 260., 4), (2, 3));
    }

    #[test]
    fn band_hi_beyond_total_clamps_to_n() {
        // deltas 使 total 增大到 400，band 超高 → i1 夹到 n 而不是多算。
        let base = blk(vec![100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[(2, 100.)], 0., 1e9, 3), (0, 3));
    }
}
