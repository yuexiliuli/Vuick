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
use crate::markdown::parse::{BlockMeta, RowKind};
use crate::markdown::parse::inline_style::MathRegion;
use crate::markdown::wysiwyg::prewarm::PrewarmSig;

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
pub(crate) fn pass1_base_layout(
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
    // 旧尾部起点 j：首个旧块 src_line > 编辑影响区间终点（二分于旧分块布局）。
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
    // 新尾部起点 k：首个新块 src_line ≥ 旧尾部 src_line + 行数净增。旧布局缺失
    // （首建）或编辑近文末（无旧尾部）时中段扫到底。块 src_line 随下标单调 →
    // 直接**二分**首个 `src_line ≥ tail_src`（4-6ms 主成本的旧实现被分块消除）。
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
    // 中段（编辑所在 → 新尾部起点）重新估算，收成 (高, src) 描述子。
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
            // 图片行：用当前加载尺寸/宽覆写按 `shape_image_row` 同式算高（占位→真实 /
            // 拖拽改宽现在也能在「基表复用」路径由 `patch_pending_image_heights` **稀疏
            // 定向覆写**，不再强制整表重建——此公式与稀疏覆写共用 `image_block_base_height`，
            // 带内外必须同一公式防滚动跳）。块高 = 图片盒高 + 题注间隙 + 题注行高。
            let src = crate::markdown::parse::parse_image_line(doc.line_str(srcline).trim())
                .map(|(_, s)| s)
                .unwrap_or_default();
            image_block_base_height(editor, &src, avail, line_h)
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
    // **分块增量路径**：前缀保持 + middle 就地替换 + 旧 [j..) 尾部 Chunk 整块复用
    // （src 行号整体 +line_delta，只动 Chunk.off）→ 中段编辑 O(#编辑块 + C)，不再
    // O(n) 平铺重排。仅在「尾部恰好对齐」时成立。
    let tail_ok = matches!(old_n, Some(n_old) if k <= n && j <= n_old && n_old - j == n - k && keep <= n_old);
    if tail_ok {
        let old_ref = old_holder.as_ref().expect("tail_ok 蕴含旧布局在场").borrow();
        base.borrow_mut().apply_structural_edit(&old_ref, keep, &middle, j, line_delta, n);
        crate::prof::end("wy.pass1", _t_p1);
        return;
    }
    // 兜底：平铺重建（首建/尾部对齐异常；正确 > 快）。
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
///
/// `pass1_base_layout` 首建路径的昂贵处是**逐块 doc 访问**（`block_meta` +
/// `line_chars_ascii` + mermaid 源组装 + 图片源解析），而这些输入全部与主题/宽度无关。
/// 后台 `open_from_rope`（无窗口/主题）在解析重叠期先把每块这类输入算成
/// [`PrebuildPass1`]（`Send`）随结果带回；首帧 `build_frame` 看到预建槽非空且守卫通过
/// （未编辑 `struct_ver==0`、无 mermaid 缩放 `zgen==0`、块数齐）→ [`pass1_from_prebuild`]
/// 用**真实** `WyTheme`/`avail` 以 O(n) 纯算术物化 `BlockLayout`（160k 块 ~ms 级），
/// 替代首帧对全篇 doc 估高（Step1 基线 debug ~103ms / release ~29ms）。守卫不通过（已
/// 编辑/已缩放/宽度换代等）→ 走既有 `pass1_base_layout` 兜底（正确优先）。与 Step1 的
/// rope/chunk 路径共存：预建只读 `Doc` 公开查询（`block_meta`/`line_chars_ascii`/
/// `line_str`），不触碰 `Doc::open_rope` 的行装配/零拷贝实现，也不改 `scan_blocks`。
pub(crate) fn build_prebuild_pass1(doc: &Doc) -> PrebuildPass1 {
    let _t = crate::prof::begin();
    let n = doc.block_count();
    let mut blocks = Vec::with_capacity(n);
    // 需要行长统计（chars/ascii）的块：Code/Table/Image 的 `estimate_height` 只用 `aux`
    // /源文本，不需要首行行长（与 `est_block` 的 else 分支一致，对拍口径同源）。
    let mut need_chars: Vec<(usize, usize)> = Vec::new(); // (块下标, 首行源行)
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
                // mermaid 围栏判定：与 est_block 同式（读首行 `line_str`，副本次数 =
                // 代码块数，占比低）。
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
                    crate::markdown::parse::parse_image_line(doc.line_str(srcline).trim())
                        .map(|(_, s)| gpui::SharedString::from(s.to_string()))
                        .unwrap_or_default(),
                );
            }
            // Code/Table/Image 的 `estimate_height` 只用 aux/源文本，不需要首行行长。
            RowKind::Table => {}
            _ => need_chars.push((i, srcline)),
        }
        blocks.push(pb);
    }
    // 行长统计用**对 rope 的单趟 chunk 顺序扫描**融合计算（零每块 RopeSlice 定位，与
    // `build_line_stats_utf16_rope` 同构）——对 6.4MB/139k 块省 ~每块一次 chunk 二分
    // （实测单块 line_chars_ascii 的 rope 切片开销 ~2.4× Str 版）。
    fill_prebuild_chars(doc, &mut blocks, &need_chars);
    crate::prof::end("wy.prebuild", _t);
    PrebuildPass1 { blocks }
}

/// 单趟 chunk 顺序扫描填 `need_chars` 各块首行的 `(chars, ascii)`（零 `RopeSlice`
/// 二分；`need_chars` 按 `src_line` 升序 = 块序天然升序 → 一趟推进、命中即写）。
/// 行语义与 `Doc::line_chars_ascii` 的 rope 分支逐值一致（行尾 `\n` 停、`\r` 跳过、
/// 非 ASCII 按引导字节折算），对拍测试锁死。
fn fill_prebuild_chars(doc: &Doc, blocks: &mut [PrebuildBlock], need: &[(usize, usize)]) {
    if need.is_empty() {
        return;
    }
    let rope = doc.rope();
    let mut ni = 0usize;
    let (mut bi, mut tgt) = need[ni];
    // 当前字节所在源行：逐 `\n` 推进；`started` = 正在计 tgt 行内容。
    let mut cur_line = 0usize;
    let mut started = false;
    let mut line_chars = 0u32;
    let mut line_ascii = true;
    for chunk in rope.chunks() {
        let bytes = chunk.as_bytes();
        let mut j = 0usize;
        let m = bytes.len();
        while j < m {
            // 每次进线先判：当前行（`cur_line`）是否已到下一目标行首。
            if !started && cur_line == tgt {
                started = true;
                line_chars = 0;
                line_ascii = true;
            }
            let b = bytes[j];
            if started {
                if b == b'\n' {
                    // 行尾：落盘并切到下一个待填块（同一行相邻块由顶部判进入）。
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
                    continue; // 与 line_chars_ascii 一致（\r 不计入）
                }
                line_chars += 1;
                if b >= 0x80 {
                    line_ascii = false;
                    // 按 UTF-8 引导字节推进（同 `build_line_stats_utf16_rope` 口径；
                    // 连续字节 0x80..C0 按 1 推进防死循环）。
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
    // 末尾：目标为最后一行且文末无尾 `\n` → 补记。
    if started && ni < need.len() {
        blocks[bi].chars = line_chars;
        blocks[bi].ascii = line_ascii;
    }
}

/// 单个预建块的估算高——与 [`pass1_base_layout`] 的 `est_block` **同一公式**（mermaid
/// 卡高 / 图片 `image_block_base_height` / 其余 `estimate_height`），保证首帧物化结果与
/// 既有首建 path 逐值一致（对拍测试锁）。mermaid 卡高按 zoom=1 预建（冷开守卫
/// `zgen==0`；缩放后再重建即按真 zoom 走既有 path）；图片现场读 `image_sizes`（空 =
/// 占位高，与 `patch_pending_image_heights` 稀疏覆写同公式）。
pub(crate) fn prebuild_block_height(
    editor: &WyEditor,
    pb: &PrebuildBlock,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> f32 {
    if let Some(src) = &pb.mermaid {
        f32::from(mermaid_card_height(src))
    } else if pb.kind == RowKind::Image {
        let src: &str = pb.image_src.as_deref().unwrap_or_default();
        image_block_base_height(editor, src, avail, line_h)
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
pub(crate) fn pass1_from_prebuild(
    editor: &WyEditor,
    base: &Rc<RefCell<element::BlockLayout>>,
    theme: &WyTheme,
    avail: f32,
    line_h: Pixels,
) -> bool {
    let _t = crate::prof::begin();
    {
        let b = editor.prebuild_slot.borrow();
        let Some(p) = b.as_ref() else { return false };
        if p.len() != editor.doc().block_count() {
            return false;
        }
        let mut heights = Vec::with_capacity(p.len());
        let mut src_lines = Vec::with_capacity(p.len());
        for pb in &p.blocks {
            heights.push(prebuild_block_height(editor, pb, theme, avail, line_h));
            src_lines.push(pb.src_line);
        }
        base.borrow_mut().rebuild_from_flat(heights, src_lines);
    }
    *editor.prebuild_slot.borrow_mut() = None;
    crate::prof::end("wy.pass1_prebuilt", _t);
    true
}

/// 在可视带内二分定位 [i0, i1) 块区间。
///
/// **简要**：用每段的「有效累计高度」做二分，找出当前这一屏框住的是哪些段落块
/// （返回下标区间 [i0, i1)）——后面只在屏内这几块上做昂贵的事，屏外全用粗算高度占位。
///
/// `eff_top(i)` = 基础前缀 + 动态增量前缀；单调 → 二分 `count_true` 得区间。
/// `delta_pref` 为**调用方提供的**增量前缀和表（`FoldPlan.delta_pref` 跨带缓存，随
/// `deltas` 同源构建），滚动帧免每帧新建 `Vec<f32>` 做 O(k) 分配与填充。
fn find_band_range(
    base: &Rc<RefCell<BlockLayout>>,
    deltas: &[(usize, f32)],
    delta_pref: &[f32],
    band_lo: f32,
    band_hi: f32,
    n: usize,
) -> (usize, usize) {
    let _t_bis = crate::prof::begin();
    // `delta_pref[p]` = 前 p 项增量之和（p ∈ [0, len]），一次 O(k) 建成 → 任意
    // `eff_top(i)` 只需 O(log len) 定位（partition_point 找首个 `ix >= i`），旧实现每个
    // 二分步都对前缀现算 O(len) 求和：带内 3000 条增量时 bisect ~2.8ms/帧、Pass3 逐行
    // O(rows×len) ~27ms/帧。前缀表现由调用方缓存复用（见 `build_frame`）。
    debug_assert_eq!(delta_pref.len(), deltas.len() + 1, "前缀和表须与增量表对齐");
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
            // A1「命中先行」+ 只物化 miss/caret 块（连续段合并批量 parse）。
            let (need_row, mut hit_map) =
                band_hit_test(editor, doc, base, lo, hi, caret_line, shape_key);
            let mut rows_map = band_parse_rows(doc, &need_row);
            // 整带顺序装配（Pass3 依赖 `shaped` 与块序一致）：命中 → 浅克隆
            // （heavy 数据都是 Arc/Rc，top 在 Pass3 现设）；否则从物化行 shape。
            let _t_shape = crate::prof::begin();
            for k in lo..hi {
                let cid = block_content_id(doc, k);
                let mut srow = if let Some(rc) = hit_map.remove(&k) {
                    (*rc).clone()
                } else if is_giant_code_block(doc, base, k) {
                    // I4 巨型代码块快速路径：不物化、不整块重 shape 出权威行——复用
                    // 钉住的陈旧行（内容滞后至上次 settle，几何/字节图仍可用）或估计高
                    // 占位；登记待静止 settle（`giant_fast_row` 内部完成）。
                    giant_fast_row(editor, doc, base, k, shape_key)
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
                                let rc = editor.shape_rows.borrow_mut().put(cid, shape_key, srow);
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
                        // ── M5-B4 决策：**保留**（先发防御，非「把回退成功当完成」的短路）──
                        // 它防的是「回退成功但一手字形是错误占位」被直接缓存钉住——删掉会把
                        // caret/非 caret 翻转回归。本机无法稳定构造 COLR 失败真机场景 → 以
                        // 静态结论保留、未真机实测；如需评估去留：WY_NOTDEF_DEBUG=1 观察
                        // pass2 fallback-verify 是否仍触发 + 二次重排 run 字体/glyph 稳定，
                        // 稳定即可考虑裁剪（见 cache.rs `NotdefRetry` 的 M4 归档注释）。
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
                            let rc = editor.shape_rows.borrow_mut().put(cid, shape_key, srow);
                            (*rc).clone()
                        }
                    }
                };
                // 命中缓存 / 巨块 / 冷却复用的行，其 `src_line`/`src_lines` 是**首次命中块**
                // 钉死的身份（`shape_docrow` 取该块 `r.src_line`/`r.lines`；缓存指纹键
                // `block_content_id` 不含 `src_line`）——文本完全相同的不同块共享指纹，
                // 复用行身份串到首块 → 源偏移反查（hit）与选区求交（paint）都算错行。
                // 在复用点按当前块 k 重贴权威身份（覆盖区间与 `assemble_docrow`/巨块占位
                // 同一口径：`src_line(k) .. src_line(k+1)`，末块 `line_count()` 兜底）。
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
                debug_special_row(editor, window, srcl, &srow, base_font);
                base.borrow_mut().set(k, f32::from(srow.height));
                shaped.push((k, srow));
            }
            crate::prof::end("wy.bf.shape", _t_shape);
        }
    crate::prof::end("wy.pass2", _t_p2);
    shaped
}

/// A1+「命中先行」（内容寻址）：整带内判哪些块**不需要**物化 DocRow——命中 = 缓存
/// **内容指纹**键匹配且非 caret 行（caret 行 shape 依赖光标，即便已在缓存也必须强制物化
/// 重 shape、不缓存；逐帧至多一行）。结构编辑后位移但内容未变的块指纹不变 → 命中旧行；
/// 只有内容真正变化的块才 miss。只对「未命中 ∪ caret 行」批量物化（连续段合并复用批量
/// parse）。本函数内部临时借用 `shape_rows` 缓存做命中判定，返回 (need_row, hit_map)。
#[allow(clippy::type_complexity)]
fn band_hit_test(
    editor: &WyEditor,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    lo: usize,
    hi: usize,
    caret_line: usize,
    shape_key: u64,
) -> (Vec<usize>, std::collections::HashMap<usize, std::rc::Rc<ShapedRow>>) {
    let mut row_cache = editor.shape_rows.borrow_mut();
    // notdef/回退自愈判决（settled/verified/attempts）只应随**真实代**（内容/图片/数学/
    // mermaid 缩放）变化而作废。`shape_key` 只含「宽度 ^ mermaid 缩放」——纯宽度换代
    // （如点击清帧后 measure 以 800/806 两宽反复建帧）不代表字形解析换代：回退/notdef
    // 判决与折行宽无关。旧实现 `key_mismatch` 直接作废判决，导致点击书尾注释章时 800 宽
    // 建帧把整章已校验的回退判决清掉 → 下一 806 建帧重新走 `fallback-verify`「首排不缓存
    // →冷却→次排」，而首排回退字形宽不稳定（实测 [!90] 行 776 vs 782，一个字形 advance
    // 差 6px）→ 同一文本在相邻帧按两种 wrap 显示（句号跨行翻转＝用户所见抖动）。
    let gen_changed = editor.frame.borrow().as_ref().is_some_and(|f| {
        (f.rgen, f.igen, f.mgen, f.zgen)
            != (
                editor.content_gen(),
                editor.images_gen(),
                editor.math_gen(),
                editor.mermaid_zoom_gen(),
            )
    });
    let mut need_row: Vec<usize> = Vec::new();
    let mut hit_map: std::collections::HashMap<usize, std::rc::Rc<ShapedRow>> =
        std::collections::HashMap::new();
    for k in lo..hi {
        // I4 巨型代码块：**不进 need_row / hit_map**——物化 + 逐行 shape 整块代价可达
        // 秒级（debug 6MB 单围栏 1.4s），由 Pass2 组装循环走快速路径（钉住陈旧行 /
        // 估计高占位），静止后 `settle_giant_blocks` 权威补真。
        if is_giant_code_block(doc, base, k) {
            continue;
        }
        let srcline = base.borrow().src_line(k) as usize;
        let cid = block_content_id(doc, k);
        if caret_line == srcline {
            need_row.push(k);
        } else if let Some(rc) = row_cache.get(cid, shape_key) {
            if shaped_row_has_notdef(&rc) && !editor.notdef_settled(k) {
                // 命中但含 .notdef（此前冷启动坏字形被缓存/预热写入）：视作 miss，强制以
                // 新鲜平台重排——否则豆腐块被旧缓存钉住整次运行。已 `settled`（判定确无
                // 字形）的块例外：tofu 即正确显示，正常命中。
                need_row.push(k);
            } else {
                hit_map.insert(k, rc);
            }
        } else {
            // 缓存 miss。**真实代**变化（图片解码完成 igen / 内容编辑 rgen / 数学就绪
            // mgen 等，见上方 `gen_changed`）时，旧条目的 `settled/verified/attempts` 判决
            // 可能对应退化窗口 → 作废让本块按新代重新校验/重试（换代自愈）；纯宽度换代
            // 不作废（字形解析与折行宽无关，见 `gen_changed` 注释——否则点击重建触发
            // fallback-verify 冷却重装 → 首排不稳定宽度两态翻转）。
            if gen_changed && row_cache.key_mismatch(cid, shape_key) {
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

/// I4 巨型代码块**快速路径行**：不物化、不整块重 shape 出权威行，只给「可显示」近似。
///
/// 优先级：① `shape_rows` 缓存命中（早前 settle 的权威行）→ 直接复用不登记；
/// ② 钉住的陈旧权威行（编辑作废 `shape_rows` 后仍可用，几何/字节图完好 → 巨块内键入
/// 期间文本/光标/点击继续工作，内容滞后至上次 settle）→ 复用 + 登记待 settle；
/// ③ 首现/未知 → 估计高占位（空面板，内容静止 settle 后权威出现）+ 登记待 settle。
/// ②③ 登记进 [`Prewarm::pending_giant`] 后由 [`self::settle_giant_blocks`] 静止期补算
/// 真整形并 notify——每键不再整块重 shape。
fn giant_fast_row(
    editor: &WyEditor,
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
    shape_key: u64,
) -> ShapedRow {
    let cid = block_content_id(doc, k);
    // ① 权威行已在缓存（settle 已落盘，本帧复用）→ 既快又新，不需重新登记。
    if let Some(rc) = editor.shape_rows.borrow_mut().get(cid, shape_key) {
        return (*rc).clone();
    }
    // ② 钉住的上一次权威行（几何/字节图仍可用）→ 复用 + 登记待 settle。
    let stale = {
        let pw = editor.prewarm.borrow();
        pw.giant_stale(cid)
    };
    if let Some(rc) = stale {
        editor.prewarm.borrow_mut().register_giant(k);
        return (*rc).clone();
    }
    // ③ 首现/未知：估计高占位 + 登记待 settle。
    editor.prewarm.borrow_mut().register_giant(k);
    giant_placeholder_row(editor, doc, base, k)
}

/// I4 巨块估计高占位：仅面板背景 + 基础布局当前估计高（不物化、不逐行 shape）。
/// 代码块 `estimate_height = code_line()*(aux+2)` 与真实（nowrap 逐行等高）一致数量级，
/// 滚动条量程/带边基本不抖；`editable=false` → caret/命中未知几何时不画光标不引导错位。
fn giant_placeholder_row(
    editor: &WyEditor,
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
        lh: editor.theme.code_line(),
        x_off: px(crate::config::markdown::CONTENT_EDGE_PAD_X_PX),
        align_offsets: Vec::new(),
        height: px(height),
        top: px(0.),
        kind: IrKind::Code,
        table: None,
        table_edit: None,
        image: None,
        mm: None,
        extra: RowExtra::default(),
    }
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
    let math_start = &plan.sets.math_start;
    let math_body = &plan.sets.math_body;
    let fold_lines = &plan.sets.fold_lines;
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
/// 块的**内容指纹**（整形缓存内容寻址键）：结构性编辑（Enter/合并/围栏）后，位移但
/// 内容未变的块指纹不变 → Pass2 命中旧整形结果，免整带重排（结构 Enter 的带重整形
/// ~6ms debug → 仅被编辑块重排）；只有内容真正变化的块才 miss。掺 kind/跨度/首末行
/// 文本；同指纹碰撞要求「同 kind、同跨度、同首行、同末行」的**不同**块——视觉无碍。
pub(crate) fn block_content_id(doc: &Doc, block: usize) -> u64 {
    let Some(m) = doc.block_meta(block) else {
        return block as u64; // 无 meta（安全带）：退化为按下标，永不跨编辑命中
    };
    use std::hash::Hasher as _;
    let mut h = rustc_hash::FxHasher::default();
    let kind = match m.kind {
        crate::markdown::parse::RowKind::Paragraph => 1u64,
        crate::markdown::parse::RowKind::Heading(_) => 2u64,
        crate::markdown::parse::RowKind::Code => 3u64,
        crate::markdown::parse::RowKind::Quote => 4u64,
        crate::markdown::parse::RowKind::Bullet => 5u64,
        crate::markdown::parse::RowKind::Rule => 6u64,
        crate::markdown::parse::RowKind::Table => 7u64,
        crate::markdown::parse::RowKind::Image => 8u64,
        crate::markdown::parse::RowKind::Blank => 9u64,
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
// 超大块（单围栏 6MB：debug 单次权威整形 ~1.4s）进带/编辑时绝不能在 Pass2 同步整块重
// shape——首现/滚动进带先走「快速路径」（钉住陈旧行 / 估计高占位），登记进
// [`Prewarm::pending_giant`]，静止后由 [`self::settle_giant_blocks`] 补算真整形并 notify。
// 阈值与 `prewarm_band` 的「大块跳过预热」同源（`block_span` 双阈值），见其下方调用点。
pub(crate) const GIANT_SPAN_LINES: usize = 64;
pub(crate) const GIANT_SPAN_BYTES: usize = 4096;

/// 块跨度 `(覆盖源码行数, 字节跨度)`——I4 巨块判定与 `prewarm_band` 大块跳过**共用**
/// 同一计算源（对齐阈值：改一个常量两处同时生效）。行数不反映行的宽度（胖表格多列），
/// 字节更准，双阈值取或。
pub(crate) fn block_span(
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

/// **巨型代码块**判定：`kind == Code` 且跨度超阈值（与 `prewarm_band` 大块跳过同源）。
/// 命中 → `band_hit_test` 不物化、Pass2 走 [`self::giant_fast_row`]，静止后权威整形。
pub(crate) fn is_giant_code_block(
    doc: &Doc,
    base: &Rc<RefCell<BlockLayout>>,
    k: usize,
) -> bool {
    let (span_lines, span_bytes) = block_span(doc, base, k);
    doc.block_meta(k).is_some_and(|m| m.kind == RowKind::Code)
        && (span_lines > GIANT_SPAN_LINES || span_bytes > GIANT_SPAN_BYTES as i64)
}

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
    // 帧内衬 = 内容盒内衬**单一来源** `CONTENT_EDGE_PAD_X_PX`（与预览行内 px_3 同源；
    // 命中/光标/划线/拖拽几何全部经 `frame.padding` 推导，改此值全站一致）。
    let padding = px(crate::config::markdown::CONTENT_EDGE_PAD_X_PX);
    let rgen = editor.content_gen();

    if n == 0 {
        return empty_frame(editor, line_h, padding, rgen, band, width);
    }

    let (band_lo, band_hi) = band;
    let avail = (f32::from(width) - f32::from(padding) * 2.).max(1.);
    let width_bits = f32::from(width).to_bits();
    let igen = editor.images_gen();
    let mgen = editor.math_gen();

    // 基础布局：代 = `(struct_ver, 宽度, mermaid缩放)`——**不含 `images_gen`/`mgen`**
    // （图片/数学高变更走稀疏定向覆写 / fold_plan 稀疏 delta，见 `height_epoch` eval）。
    // 滚动/带移动与**非结构性编辑**都复用（编辑只对受影响块做 Fenwick 点更新），不再
    // 每档/每键对整个 O(n) 高度数组重算前缀和（旧实现在内容代变化时对全篇重扫 ~400ms，
    // 是「一编辑就卡」之一）。
    let _t_setup = crate::prof::begin();
    let epoch = super::height_epoch(editor.struct_ver(), width_bits, editor.mermaid_zoom_gen());
    let (rebuilt, base, old_layout) = editor.ensure_block_layout(n, epoch);
    // 非结构性编辑：基础布局复用，只修补受影响块高（估计 → Fenwick 点更新，O(log n)）。
    if !rebuilt {
        if let Some(ixs) = editor.take_pending_patch_blocks()
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
        // 图片占位→真实 / 拖拽改宽：**不动 `height_epoch`**（免拖拽每 tick 近全量 Pass1），
        // 只对受影响的含图块做**稀疏 Fenwick 点更新**（公式与 pass1 共用
        // `image_block_base_height`）。必须在 `find_band_range`/Pass2 之前（二分与行高都读
        // 新基础高）——位置见 `patch_pending_image_heights` 收录端（`set_image_size`/
        // `set_image_width`）。
        editor.patch_pending_image_heights(&base, avail, line_h);
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
        // Step4-①：首帧有后台预建输入（`install_open` 存槽）且守卫通过（未编辑 /
        // 未 mermaid 缩放 / 块数齐）→ O(n) 纯算术物化（~ms），首帧不再全量 doc 估高。
        // 守卫不通过（已编辑/已缩放/块数不齐等，含宽度换代重建）→ 既有 path 兜底。
        let prebuilt = editor.struct_ver() == 0
            && editor.mermaid_zoom_gen() == 0
            && editor
                .prebuild_slot
                .borrow()
                .as_ref()
                .is_some_and(|p| p.len() == n);
        if !(prebuilt && pass1_from_prebuild(editor, &base, theme, avail, line_h)) {
            // 预建不可用（守卫不通过/块数不齐）→ 弃槽（避免悬置 ~5MB）走既有 path。
            *editor.prebuild_slot.borrow_mut() = None;
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
    // 有效增量：无 reveal（常见：光标在普通行）→ 直接复用计划 deltas **及其跨带缓存
    // 的前缀和表**（零拷贝、滚动帧免每帧 O(k) 重建）；有 reveal → 过滤掉被归零的块
    // （最多几个），每带一次 O(k) 拷贝 + 就地重建前缀（reveal 帧罕见且表小）。
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

    // 带内块区间 [i0, i1)：用「有效前缀」（基础 + 动态增量，单调）二分。
    let (i0, mut i1) = find_band_range(&base, &deltas, &delta_pref, band_lo, band_hi, n);

    // Pass 2：带内非折叠/非数学块 shape，真实高 `set` 回基表（带外沿用收敛真值）。
    // reveal 覆盖：caret 所在区域的行不再 blocked（还原源码 shape）。
    // 行整形缓存（滚动/带重建复用）：键 = **epoch（不含 `mgen`）**；caret 严格落在
    // 含行内公式的文本行时（reveal，shape 依赖光标）该行不缓存；编辑作废由
    // `replace_range` 负责，公式就绪对 `$$` 卡 / 行内 spacer 的换代由 `on_math_ready`
    // **定向作废**该行——不再让整带无关块随每个公式完成全 miss 重排（对齐 code 修复）。
    // 行整形缓存键（内容寻址，见 `block_content_id`）：**只含宽度与 mermaid 缩放**——
    // 内容变化由每块的指纹 miss 覆盖、图片就绪/字号/主题由 clear 覆盖、数学就绪由
    // 定向作废覆盖。**不含 `struct_ver`**：结构编辑（Enter/合并）后位移但内容未变的块
    // 指纹不变 → 命中旧行，免整带重排（这是「结构编辑后带重整形 ~6ms」的架构根因）。
    let shape_key = (width_bits as u64) ^ editor.mermaid_zoom_gen();
    let mut shaped = pass2_shape_band(
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
    // 带覆盖自校正（滚动闪烁 / 首屏缺内容根治）：`find_band_range` 按**估算高**二分截断
    // 带内块区间；Pass2 边 shape 边把**真实高** `set` 回 base（见 pass2 末尾）。估算过大于
    // 真实（大块首屏尤甚）时，截断处 base 仍是估算 → 物化行底（真值坐标）远低于带高
    // `band_hi` → 带下方一屏没有行可画：滚动时视口整屏空白（用户所见「内容出现后闪烁
    // 消失」）、打开时下半屏缺内容。这里以已校正的 base 重算 `i1` 并补齐新块，直到物化
    // 窗真值底到达 `band_hi` 或已到文末——在**同一帧 build 内**收敛，不产生中间空白帧。
    // 收敛：真实高写回后 `find_band_range` 的上界单调不减（块数上界 n）；`CAP` 兜底
    // 过估极端文档（每轮都向前推进带内块数，16 轮远超实际需要）。
    const BAND_COVER_CAP: usize = 16;
    let mut band_cover_steps = 0;
    while i1 < n && band_cover_steps < BAND_COVER_CAP {
        band_cover_steps += 1;
        // 用「Pass2 已写回真实高后的 base」重算 [band_lo, band_hi) 的带内上界。
        let (_, i1_next) = find_band_range(&base, &deltas, &delta_pref, band_lo, band_hi, n);
        if i1_next <= i1 {
            break;
        }
        // 补齐 [i1, i1_next)：整带重跑 shape（前段命中整形缓存，代价≈仅新块），以覆盖
        // [i0, i1_next) 的最新输出为准；base 再被新块写回真实高，下轮继续收窄差距。
        i1 = i1_next;
        shaped = pass2_shape_band(
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
    }

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

/// 空文档（0 块）占位帧：单个空行 ShapedRow + 单元素 BlockLayout，量程 = 单行高。
#[allow(clippy::too_many_arguments)] // 构建空帧所需的既有上下文参数
fn empty_frame(
    editor: &WyEditor,
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
        igen: editor.images_gen(),
        mgen: editor.math_gen(),
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
/// - **静置门控**（Sprint 4b）：滚动/内容/宽度全未变且连续 [`super::Prewarm::IDLE_SKIP_FRAMES`]
///   帧无滚动意图 → 跳过整形循环（只保留上方登记簿）——预整形只在滚动活跃期工作，
///   滚动停稳/纯编辑帧不再每帧白烧 8ms 预算；滚动/缩放/内容变化下一帧立即复原。
///   跳过仅影响后续带穿越的冷启动（届时若正在滚动已恢复，Pass2 补一次整形突发）。
///   `scroll_top` 必须是 prepaint 的**未量程化**滚动像素：`frame.band` 是 512px 量子栅格，
///   带内几像素的微滚不动带，签名若不区分会把「慢速滚动中」误判为静置（见
///   [`super::PrewarmSig`]）——跨带时落点带冷、Pass2 突发回归。
pub(crate) fn prewarm_band(
    e: &WyEditor,
    frame: &Frame,
    window: &mut Window,
    scroll_top: f32,
) {
    // I4：巨型代码块静止权威整形（先于预热登记簿；独立静止门控，静止期才干活）。
    settle_giant_blocks(e, window, frame, scroll_top);
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
    // 静置门控（Sprint 4b）：登记簿已新鲜（上方带序号/dir 更新），这里看本帧相对上一帧
    // 有没有滚动/缩放/内容意图——全无且连续 N 帧 → 跳过预整形循环（O(1) 开销收工）。
    if st.idle_skip(frame, scroll_top) {
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
    // shape_key 与 `build_frame` 完全同式（**宽基**：width_bits ^ mermaid_zoom_gen；
    // 不含 mgen/struct_ver——内容变化由内容指纹 miss、公式就绪走定向作废，见
    // `build_frame` 的 `block_content_id` 说明）。
    let width_bits = f32::from(frame.width).to_bits();
    let sk = (width_bits as u64) ^ e.mermaid_zoom_gen();
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
        let srcline = frame.base.borrow().src_line(k) as usize;
        // 大块跳过预热：单块 DocRow 覆盖整块源区间，`rows_in_block_window(k,k)` 会
        // 一次性物化并整形整个块（debug 胖表格/长代码段可达 1.6s/块，实测快滚跨界时
        // 单帧窒息）。预热只挑小/中块；大块进带时由 Pass2 权威整形（那时本来就要渲染，
        // 且 I4 后巨型代码块改走快速路径 + 静止 settle）。判定用「字节跨度 + 行跨度」
        // 双阈值——行数不反映行的宽度（胖表格多列），字节更准；与 I4 巨块闸共用
        // [`self::block_span`]（阈值常量同源）。
        let (span_lines, span_bytes) = block_span(doc, &frame.base, k);
        // 中大型代码块（≥8 行）也按「大块」跳过预热：syntect 整块解析是 debug 下
        // 的主要单块成本（实测 14 行 rust 块 ~300ms），预热 8ms 预算拦不住它——
        // 交进带时 Pass2 权威整形（那时本来要渲染），滚动帧不被投机整形拖垮。
        let is_medium_code = doc
            .block_meta(k)
            .map(|m| m.kind == RowKind::Code && span_lines > 8)
            .unwrap_or(false);
        let big = span_lines > GIANT_SPAN_LINES
            || span_bytes > GIANT_SPAN_BYTES as i64
            || is_medium_code;
        // 与 Pass2 `blocked` 同判据：折叠/math 块不物化整形行。
        let blocked = match &plan {
            Some(p) => {
                p.sets.fold_lines.contains(&srcline)
                    || p.sets.math_body.contains(&srcline)
                    || p.sets.math_start.contains_key(&srcline)
            }
            None => false,
        };
        if !blocked && !big && caret_line != srcline && rc.get(block_content_id(doc, k), sk).is_none() {
            // 与 Pass2 miss 分支同式：整形成功即入缓存（随后 Pass2 命中时浅克隆复用）。
            // 含 .notdef 的坏行**不入缓存**——进带后由 Pass2 以「冷却一帧 → 新鲜重排」
            // 自愈（一律重试，不为“回退已温”静默缓存）。
            if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
                #[cfg(test)]
                e.bump_prewarm_shapes();
                let row =
                    shape_docrow(r, e, window, frame.width, avail, &e.theme, &e.theme.font);
                if !shaped_row_has_notdef(&row) {
                    rc.put(block_content_id(doc, k), sk, row);
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

/// I4 巨型代码块「静止权威整形」：Pass2 快速路径把巨块登记进 [`Prewarm::pending_giant`]
/// 后，这里在**帧签名静止**（复用 [`PrewarmSig`] 门控语义：滚动像素/宽度/内容/图片/数学
/// /mermaid 缩放任一变都复位）且连续 [`Prewarm::GIANT_SETTLE_IDLE_FRAMES`] 帧后，对本帧
/// 带内可见的一个 pending 巨块做一次**权威整形**（物化 + `shape_docrow` + 入 `shape_rows`
/// 缓存 + 更新钉 + 真高写回基础布局），并把缓存帧置 None + 请求下一帧——权威内容/高度
/// 在下帧出现。编辑/滚动活跃期不触发（每键不整块重 shape）。
///
/// 单块权威整形可达秒级（debug 6MB 单围栏 ~1.4s），**一次一块**摊到静止后的逐帧；
/// 带外（滚走）的 pending 块不强制整形，等它回带 + 静止再补。正确性闭环：首现/滚动
/// 进带 → 占位（`giant_fast_row`）→ 静止 → 这里真整形 → 下帧内容正确出现。
pub(crate) fn settle_giant_blocks(
    e: &WyEditor,
    window: &mut Window,
    frame: &Frame,
    scroll_top: f32,
) {
    let mut st = e.prewarm.borrow_mut();
    if st.pending_giant.is_empty() {
        // 无 pending → 静止整形状态归零（换源/清空后不残留旧签名）。
        st.giant_idle = 0;
        st.giant_sig = None;
        return;
    }
    let sig = PrewarmSig::of(frame, scroll_top);
    if st.giant_sig != Some(sig) {
        // 签名变化（滚动/缩放/内容/宽度动过）→ 复位静止计数，等它真正停稳。
        st.giant_idle = 0;
        st.giant_sig = Some(sig);
    } else {
        st.giant_idle = st.giant_idle.saturating_add(1);
    }
    if st.giant_idle < Prewarm::GIANT_SETTLE_IDLE_FRAMES {
        // I4 补帧：登记待 settle 后**滚动/编辑停止即不再有事件帧**——「静止连续帧」计数
        // 会卡在低值，「占位 → 静止 → 权威整形」闭环永远走不到，巨块停在空占位/钉住旧
        // 行直到用户再互动。这里每帧请求下一帧，让计数推进到阈值；settle 消费完
        // `pending_giant`（`is_empty` 分支）或带判定放弃（`giant_idle=0` 分支）后不再
        // 请求 → 收敛即静止，不引入持续逐帧重绘（与 `advance_pending_scroll` 同款 RAF，
        // prepaint 内合法）。
        #[cfg(not(test))]
        window.request_animation_frame();
        return;
    }
    // 越界（换源/结构编辑后旧下标失效）的登记直接清掉。
    let n = e.doc().block_count();
    st.pending_giant.retain(|&k| k < n);
    // 只 settle 本帧带内可见的 pending 巨块（一次一块：单块即可达秒级）。
    let (bi0, bi1) = frame.band_block_range(frame.band.0, frame.band.1);
    let Some(pos) = st.pending_giant.iter().position(|&k| k >= bi0 && k < bi1) else {
        // 在带外（滚走了）：本轮不强制整形，等回带 + 静止再补。
        st.giant_idle = 0;
        return;
    };
    let k = st.pending_giant.remove(pos);
    drop(st);
    let doc = e.doc();
    let sk = (f32::from(frame.width).to_bits() as u64) ^ e.mermaid_zoom_gen();
    let avail = (f32::from(frame.width) - f32::from(frame.padding) * 2.).max(1.);
    if let Some(r) = doc.rows_in_block_window(k, k).into_iter().next() {
        // 权威整形（与 Pass2 同路径）：>8 行代码首帧纯文本 + 请求后台 syntect，就绪后
        // `on_code_parse_ready` 定向重排上色——静止期多在首帧后一两帧补上。
        let row = shape_docrow(r, e, window, frame.width, avail, &e.theme, &e.theme.font);
        let cid = block_content_id(doc, k);
        // 真高写回基础布局（下次 `build_frame` 的带/量程用收敛真值；这里用 try 借用，
        // paint/prepaint 若正持 base 借用则跳过写回，缓存仍在下帧生效）。
        if let Ok(mut b) = frame.base.try_borrow_mut() {
            b.set(k, f32::from(row.height));
        }
        let rc = e.shape_rows.borrow_mut().put(cid, sk, row);
        // 更新钉：随后编辑作废 `shape_rows` 时快速路径可复用这份权威行。
        e.prewarm.borrow_mut().pin_giant(cid, rc);
        // 强制下帧重建 Frame（base 真高 / 缓存命中才生效）并请求重绘。
        *e.frame.borrow_mut() = None;
        // 请求动画帧（prepaint 内合法）：settle 后若再无事件，不请求就没有下一帧、
        // 新内容永远不显示。测试二进制无平台帧循环，`request_animation_frame` 的
        // `current_view()` 断言只在渲染相位合法——测试里由显式 build_frame/refresh 驱动，
        // 这里跳过（不影响行为断言）。
        #[cfg(not(test))]
        window.request_animation_frame();
    }
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
        Rc::new(RefCell::new(BlockLayout::new(heights, src)))
    }

    #[test]
    fn band_mid_spans_visible_blocks() {
        // 4×100px 块，band 50..250 → 覆盖块 0/1/2（块 3 top=300 在带外）。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 50., 250., 4), (0, 3));
    }

    #[test]
    fn band_start_at_boundary_includes_start_block() {
        // band 0..300：块 3 top=300 不含（< band_hi 严格），块 0 因 top<=0 计入。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 0., 300., 4), (0, 3));
    }

    #[test]
    fn band_above_all_blocks_starts_at_first() {
        // band 顶部在首块内（负区），i0 饱和到 0；只含块 0。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], -100., 50., 4), (0, 1));
    }

    #[test]
    fn band_below_all_blocks_returns_empty_span() {
        // band 在整个文档之下（滚到底）→ 空区间 (4, 4)。
        let base = blk(vec![100., 100., 100., 100.]);
        assert_eq!(find_band_range(&base, &[], &[0.], 500., 600., 4), (4, 4));
    }

    #[test]
    fn delta_shifts_later_block_tops() {
        // 块 1 增量 +50 → 块 2 有效 top = 200+50 = 250 …band 250..260 恰好罩住块 2。
        let base = blk(vec![100., 100., 100., 100.]);
        let deltas = [(1, 50.)];
        assert_eq!(
            find_band_range(&base, &deltas, &build_delta_pref(&deltas), 250., 260., 4),
            (2, 3)
        );
    }

    #[test]
    fn band_hi_beyond_total_clamps_to_n() {
        // deltas 使 total 增大到 400，band 超高 → i1 夹到 n 而不是多算。
        let base = blk(vec![100., 100., 100.]);
        let deltas = [(2, 100.)];
        assert_eq!(
            find_band_range(&base, &deltas, &build_delta_pref(&deltas), 0., 1e9, 3),
            (0, 3)
        );
    }
}
