use super::*;

use crate::markdown::parse::inline_style;
use crate::markdown::parse::recognition;
use crate::markdown::parse::BlockMeta;

/// WyEditor 编辑核心 / 撤销重做 / 区域导航 / 光标移动（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /* ---------- 光标 ↔ 源辅助 ---------- */

    /// 源字节偏移 → (源行, 行内字节偏移)。用 `Doc::line_starts` 二分（O(log n)），
    /// 不再每帧线性扫整篇源文本（7MB 大文档每帧 ~100ms+）。
    pub(crate) fn row_col(&self, off: usize) -> (usize, usize) {
        let off = off.min(self.len());
        let ls = self.doc.line_starts();
        // 最后一个 `line_starts[i] <= off` 的行。
        let line = ls.partition_point(|&st| st <= off).saturating_sub(1);
        let start = ls.get(line).unwrap_or(0);
        (line, off - start)
    }

    /// 源行 → 该行起始字节（O(log n)，`line_starts` 二分）。
    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.doc.line_starts().get(line).unwrap_or(self.len())
    }

    /// 源行 → 行尾字节（不含换行符；`line_starts[line+1]-1` = 行尾 `\n` 位置）。
    /// 末行（无换行）返回内容尾。`CRLF` 下返回 `\n`（含 `\r`，与原文语义一致）。
    pub(crate) fn line_end(&self, line: usize) -> usize {
        let ls = self.doc.line_starts();
        let start = ls.get(line).unwrap_or(self.len());
        match ls.get(line + 1) {
            Some(next) => (next.saturating_sub(1)).min(self.len()).max(start),
            None => self.len(),
        }
    }

    /// 该行的粗略 `RowKind`（行首启发，实体内部导航用；渲染以 `DocRow` 为准）。
    fn kind_for_line(&self, line: usize) -> crate::markdown::parse::RowKind {
        use crate::markdown::parse::RowKind;
        let start = self.line_start(line);
        let end = self.line_end(line);
        let raw = self.doc.slice_str(start..end);
        let t = raw.trim_start();
        if t.is_empty() {
            RowKind::Blank
        } else if t.starts_with("```") {
            RowKind::Code
        } else if t.starts_with('|') {
            RowKind::Table
        } else if t.starts_with('#') {
            RowKind::Heading(t.chars().take_while(|c| *c == '#').count() as u8)
        } else if t.starts_with("![") {
            RowKind::Image
        } else if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
            RowKind::Bullet
        } else if t == "---" || t == "***" || t == "___" {
            RowKind::Rule
        } else if t.starts_with('>') {
            RowKind::Quote
        } else {
            RowKind::Paragraph
        }
    }

    /// 该行「可编辑段」的源局部区间 `(lo, hi)`：`lo` = 正文源偏移，`hi` = 行尾。
    /// 非文本行 → `None`（只读/不可编辑）。代码内容行 / 表格表头与数据行整行可编辑
    /// （围栏标记行 / 表格分隔行仍 `None`）。独立图片行 → alt（`![` 之后到 `](` 之前）。
    fn editable_segment(&self, line: usize) -> Option<(usize, usize)> {
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        if self.in_block_content(line) {
            return Some((0, end - start));
        }
        let raw = self.doc.slice_str(start..end);
        // 独立图片行：可编辑段 = alt（`![` 之后到 `](` 之前），题注行可编辑。
        if self.kind_for_line(line) == crate::markdown::parse::RowKind::Image {
            return self.image_alt_segment(line);
        }
        let seg = crate::markdown::parse::text_segment(&raw, self.kind_for_line(line))?;
        Some((seg.0, raw.len()))
    }

    /// 独立图片行的可编辑段（源局部区间，相对行首）：alt 内容 `[lo, hi)`。
    /// 非图片行 / 解析失败 → `None`。与 `DocRow.byte_map`（图片题注映射）同构。
    fn image_alt_segment(&self, line: usize) -> Option<(usize, usize)> {
        if self.kind_for_line(line) != crate::markdown::parse::RowKind::Image {
            return None;
        }
        let start = self.line_start(line);
        let end = self.line_end(line);
        if end <= start {
            return None;
        }
        let raw = self.doc.slice_str(start..end);
        let lead = raw.len() - raw.trim_start().len();
        let r = crate::markdown::parse::parse_ref(raw.trim_start())?;
        let lo = lead + 2;
        Some((lo, lo + r.alt.len()))
    }

    /* ---------- 可编辑块区域（代码/表格 + 围栏行判定） ---------- */

    /// 从块索引重建每个可编辑块区域（代码内容行 / 表格表头+数据行）。文本变更后调用。
    /// 结构编辑后**派生区域行号平移**（B3 增量，替代整篇 `inline_style::math_regions`/
    /// `alert_regions`/`heading_fold_regions` 重扫）：编辑线**之上**（`end <= edit_line`）
    /// 区域完全不变；**之下**（`start > edit_line`）整体平移 `Δ`（source 内容未变，保留）。
    /// （M5-B3-1 子项 4）标题折叠体与数学/警报区域同规则平移——`heading_fold_bodies` 已在
    /// `derived` 内维护（编辑线之上不变、之下平移；编辑线落在体**内** → 内容/边界变了，
    /// 无法纯平移）。若派生未缓存、或任一区域/折叠体**跨过**编辑线 → 返回 `false`，
    /// 调用方回退整篇重建（`regions_ver++`，`derived()` 懒重建并重扫标题折叠）。
    /// Cut-1a：跨行删除/合并（`removed_nl > 0`）不再一刀切回退整建，而是显式判定
    /// ——删除的每个 `\n` 都把一行（`[edit_line+1, edit_line+removed_nl]`）整体并进上一
    /// 行；这些行的**起始标记**若是某区域的开界（`$$` 围栏 / 整行 `$$…$$` / `> [!]`
    /// 警报 / 折叠标题），一旦被前文前缀破坏，区域即被吞并/改界，纯行号平移会留下
    /// 陈旧区域 → 才回退整建。纯文本合并（无任何区域）与远离区域边界的合并直接平移，
    /// 省掉整篇 `derived()` 重建 + `text_shared()` 物化 + `fold_plan` 整建（~16ms@6MB）。
    /// `removed_nl` 为 0（纯插入）时区间为空 → 与旧行为一致。
    fn shift_derived_after_edit(
        &mut self,
        edit_line: usize,
        line_delta: i64,
        removed_nl: usize,
    ) -> bool {
        let cached = self.derived.borrow_mut();
        let Some((_, rc)) = cached.as_ref() else {
            return false;
        };
        let mut d = rc.borrow_mut();
        // 被删除换行吸收的行区间（起始边界直接落在其中 → 区域有被吞并风险）。
        let abs_starts = edit_line + 1..=edit_line + removed_nl;
        // 跨线/跨吸收区间区域：编辑线落在区域内 → 内容/边界变了，无法纯平移。
        if super::view::region_containing(&d.math_regions, |r| r.range.clone(), edit_line, true)
            .is_some()
            || d.alert_fold_regions
                .iter()
                .any(|(r, _)| r.start <= edit_line && edit_line < r.end)
            || super::view::region_containing(
                &d.heading_fold_bodies,
                |r| r.clone(),
                edit_line,
                true,
            )
            .is_some()
            // Cut-1a：某区域**起始行**落在被吸收行区间 → 起始标记可能被合并吞掉。
            || d.math_regions
                .iter()
                .any(|r| abs_starts.contains(&r.range.start))
            || d.alert_fold_regions
                .iter()
                .any(|(r, _)| abs_starts.contains(&r.start))
            || d.heading_fold_bodies
                .iter()
                .any(|r| abs_starts.contains(&r.start))
        {
            return false;
        }
        let shift = |x: usize| -> usize { (x as i64 + line_delta).max(0) as usize };
        // 三处「行号 > edit_line 平移」（数学区域 / 警报折叠区 / 标题折叠体）同规则——
        // 因各自容器形态不同（MathRegion.range / (Range,_) / Range）保留显式循环，
        // 平分逻辑一致：`start > edit_line` 才平移两端（跨编辑线已由上方
        // `region_containing` 排除）。
        for reg in &mut d.math_regions {
            if reg.range.start > edit_line {
                reg.range.start = shift(reg.range.start);
                reg.range.end = shift(reg.range.end);
            }
        }
        for (r, _) in &mut d.alert_fold_regions {
            if r.start > edit_line {
                *r = shift(r.start)..shift(r.end);
            }
        }
        if line_delta != 0 {
            // 只平移**编辑线之下**的键（区域整体下方 → 其所有行都 > edit_line，见上方
            // `region_containing(edit_line)` 已排除跨线区域）——编辑线上方的区域行号
            // 不动，误平移会让 paint 的 `alert_region_kinds` 查新行号 miss（callout 左条
            // 掉色）。旧实现无条件平移全部键（合并走整建掩盖了此 bug；Cut-1a 让合并可
            // 平移后暴露，一并修正）。
            let old = std::mem::take(&mut d.alert_region_kinds);
            d.alert_region_kinds =
                rustc_hash::FxHashMap::with_capacity_and_hasher(old.len(), Default::default());
            for (l, k) in old {
                if l > edit_line {
                    d.alert_region_kinds.insert(shift(l), k);
                } else {
                    d.alert_region_kinds.insert(l, k);
                }
            }
        }
        for hb in &mut d.heading_fold_bodies {
            if hb.start > edit_line {
                *hb = shift(hb.start)..shift(hb.end);
            }
        }
        true
    }

    /// 从块元数据构造可编辑区域（代码内容行 / 表格表头+数据行）。`rebuild_block_regions`
    /// 与 `update_block_regions_after_edit`（窗口重算）共用同一构造。
    fn region_for_block_meta(m: &BlockMeta) -> Option<BlockRegion> {
        use crate::markdown::parse::RowKind;
        match m.kind {
            RowKind::Code => {
                let lo = m.src_line as usize + 1;
                let hi = lo + m.aux.max(1) as usize - 1;
                Some(BlockRegion::Code { lo, hi })
            }
            RowKind::Table => {
                let header = m.src_line as usize;
                let dlo = header + 2;
                let dhi = header + 1 + m.aux.max(1) as usize;
                Some(BlockRegion::Table { header, dlo, dhi })
            }
            _ => None,
        }
    }

    pub(crate) fn rebuild_block_regions(&mut self) {
        let n = self.doc.block_count();
        let mut regions: Vec<BlockRegion> = Vec::new();
        for i in 0..n {
            let Some(m) = self.doc.block_meta(i) else {
                continue;
            };
            if let Some(r) = Self::region_for_block_meta(&m) {
                regions.push(r);
            }
        }
        self.block_regions = regions;
    }

    /// B4：结构编辑后的 `block_regions` **增量更新**——替代每键 O(块数) 整表重建
    /// [`Self::rebuild_block_regions`]（6MB/10 万块 debug ~5.9ms/键）。
    ///
    /// 结构编辑是**局部**的：只影响「编辑线所在块」及其直接后继（Enter 分裂、退格合并、
    /// 围栏跨界至多涉及这两块）。做法：把既有区域按**所属块**（区域回到块 = 围栏开行/
    /// 表头行，再经 `line_block` 稠密查）分类——窗口 `[edit_blk, edit_blk+2)` 内 → 丢弃、
    /// 由新 doc 重算；窗口之前 → 原样保留（块内容/行号未变）；窗口之后 → 整行号平移
    /// `line_delta`。序列仍按 src_line 升序（上方保留 < 窗口新派生 < 下方平移）。
    /// 首建/整篇重置仍走 [`Self::rebuild_block_regions`]。
    fn update_block_regions_after_edit(&mut self, edit_line: usize, line_delta: i64) {
        let doc = &self.doc;
        let n = doc.block_count();
        let span = |r: &BlockRegion| -> (usize, usize) {
            match r {
                BlockRegion::Code { lo, hi } => (*lo, *hi),
                BlockRegion::Table { header, dlo, dhi } => (*header.min(dlo), *dhi),
            }
        };
        let shift = |r: &BlockRegion, d: i64| -> BlockRegion {
            let sh = |x: usize| (x as i64 + d).max(0) as usize;
            match r {
                BlockRegion::Code { lo, hi } => BlockRegion::Code {
                    lo: sh(*lo),
                    hi: sh(*hi),
                },
                BlockRegion::Table { header, dlo, dhi } => BlockRegion::Table {
                    header: sh(*header),
                    dlo: sh(*dlo),
                    dhi: sh(*dhi),
                },
            }
        };
        // 1) 旧区域按**行号**分类（不查新 doc 块：编辑线下方的行号已平移，反查会错位）：
        //    完全在编辑线上方 → 保留（内容/行号未变）；完全在下方 → 整行号平移；
        //    跨编辑线（含编辑线）→ 只可能是被编辑块的区域 → 丢弃，交给 2) 重算。
        let mut kept: Vec<BlockRegion> = Vec::with_capacity(self.block_regions.len());
        for &r in &self.block_regions {
            let (mn, mx) = span(&r);
            if mx < edit_line {
                kept.push(r);
            } else if mn > edit_line {
                kept.push(shift(&r, line_delta));
            }
            // else：跨编辑线 → 由窗口重算
        }
        // 2) 新 doc 重算受影响窗口块 [edit_blk, edit_blk+2)（Enter 分裂 / 删除合并 / 围栏
        //    ——至多涉及编辑线所在块及其直接后继两 3 块）；块序升序，按首行插回排序序列。
        let edit_blk = doc.block_index_for_line(edit_line).min(n.saturating_sub(1));
        let win_hi = (edit_blk + 2).min(n);
        for i in edit_blk..win_hi {
            let Some(m) = doc.block_meta(i) else { continue };
            let Some(r) = Self::region_for_block_meta(&m) else { continue };
            let rl = span(&r).0;
            let pos = kept.partition_point(|k| span(k).1 < rl);
            kept.insert(pos, r);
        }
        self.block_regions = kept;
    }

    /// `line` 所在的可编辑块区域（代码内容行 / 表格表头+数据行）。
    fn region_content(&self, line: usize) -> Option<BlockRegion> {
        self.block_regions.iter().copied().find(|r| match r {
            BlockRegion::Code { lo, hi } => line >= *lo && line <= *hi,
            BlockRegion::Table { header, dlo, dhi } => {
                line == *header || (line >= *dlo && line <= *dhi)
            }
        })
    }

    /// `line` 是否为可编辑块区域的**围栏行**（代码 ``` 标记行 / 表格分隔行；只读）。
    fn is_fence_line(&self, line: usize) -> bool {
        self.block_regions.iter().any(|r| match r {
            BlockRegion::Code { lo, hi } => (line + 1 == *lo) || (line > 0 && line - 1 == *hi),
            BlockRegion::Table { dlo, .. } => line + 1 == *dlo,
        })
    }

    /// `line` 是否落在可编辑块区域的**内容行**内（可编辑）。
    fn in_block_content(&self, line: usize) -> bool {
        self.region_content(line).is_some()
    }

    /// 表格内容行：取离 `col` 最近的单元格内容边界 `(start, end)`（相对行首）。
    /// 非表格行/无格 → None。
    fn table_cell_bounds(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        if !matches!(self.region_content(line), Some(BlockRegion::Table { .. })) {
            return None;
        }
        let raw = self.line_text(line);
        let cells = crate::markdown::parse::split_table_cells_mapped(&raw);
        if cells.is_empty() {
            return None;
        }
        let idx = (0..cells.len())
            .min_by_key(|&i| {
                let r = &cells[i].0;
                if col < r.start {
                    r.start - col
                } else {
                    col.saturating_sub(r.end)
                }
            })
            .unwrap_or(0);
        let r = &cells[idx].0;
        Some((r.start, r.end))
    }

    /// 表格内容行内的 caret 钳到最近单元格内容区间 `[start, end]`（不落在管道/空白上）。
    fn clamp_caret_table(&self, line: usize, col: usize) -> Option<usize> {
        let (s, e) = self.table_cell_bounds(line, col)?;
        Some(self.line_start(line) + col.clamp(s, e.max(s)))
    }

    /// 从 `line` 向下找第一个**非围栏**行（可含当前行；跳过连续围栏行）。
    fn prev_render_line(&self, line: usize) -> usize {
        let mut l = line;
        while l > 0 && self.is_fence_line(l) {
            l -= 1;
        }
        l
    }

    /// 从 `line` 向上找第一个**非围栏**行（可含当前行；跳过连续围栏行）。
    fn next_render_line(&self, line: usize) -> usize {
        let total = self.doc.line_starts().len();
        let mut l = line;
        while l + 1 < total && self.is_fence_line(l) {
            l += 1;
        }
        l
    }
    /* ---------- 编辑操作（源字节域） ---------- */

    /// 开启「编辑日志」收集（方案2：切回源码编辑区按序重放）。仅 App 对 live2 实例调；
    /// 其余实例（`md_wy` 右栏预览等）一律不开启，避免把旁路/预览增删误收进日志。
    pub fn capture_live_ops(&mut self) {
        self.live_ops.get_or_insert_with(Vec::new);
    }

    /// 取走并清空「编辑日志」（App `commit_live` 消费；此后实体随即销毁）。返回空 =
    /// 本次 live 会话无任何内容变更（等价旧的 `live2_dirty == false` 判定；二者都只由
    /// 内容变更置位，`WyEvent::Changed` 与 op 缴存同源）。
    pub fn take_live_ops(&mut self) -> Vec<(Range<usize>, SharedString)> {
        self.live_ops.take().unwrap_or_default()
    }

    /// 核心编辑：替换 `[start, end)` → `text`，重建模型、移光标、通知。
    /// 任意替换都会收敛选区（无选区）与 IME 组合区间（替换后由输入处理器重设）。
    /// **所有编辑**（输入/剪贴板/退格/任务框翻转/警报折叠）都从这里过——每笔真实
    /// 内容变更先记入撤销栈（D1）；**B2 事务合并**：连续同意图且邻接的编辑并入同一
    /// 撤销单元（连续打字/相邻退格/同点删除），只在「新事务起点」push 快照。
    pub fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        // 只读门控（A1）：`set_editable(false)` 后一律短路——输入/IME/粘贴/退格/删除/
        // Enter/撤销/重做等一切内容变更都汇聚到本函数，这里硬门控即杜绝只读预览改动。
        if !self.editable {
            return;
        }
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let _t_rr = crate::prof::begin();
        // 编辑前被替换区间（cross_newline / 标记计数 / 撤销意图用；O(被删长)）。
        let removed = self.doc.slice_str(r.clone());
        let inserted_len = text.len();
        // —— 编辑日志（方案2）：每笔真实内容变更缴存 `(字节区间, 新文本)` op，供切回
        // 源码编辑区按序重放（撤销/重做的净逆操作同样经 `apply_raw_edit` 缴存，按序重放
        // 即得最终态，见 `apply_raw_edit` 尾部同款缴存）。仅 live2 收集（`md_wy` 不开）。
        if let Some(log) = &mut self.live_ops {
            log.push((r.clone(), SharedString::from(text)));
        }
        // —— B2 撤销单元 + 事务合并（无定时器确定性判定，同 editor/undo.rs）——
        self.record_undo_unit(&r, &removed, text);
        // 原地 rope 编辑（`Doc::edit`：COW remove+insert + 增量块扫描 + 增量行起始）——
        // 不再每键拼一份 6MB 整串再分配/复制（旧 `next` String + Arc 包装是 6MB 文档
        // ~5-6ms/键的根因）；`content` 字段随之取消，rope 为唯一真源。
        let edit_line = self.row_col(r.start).0;
        // 编辑前该行内容（region 签名对比用；非结构性编辑行边界不变）。
        let old_line = self.doc.line_str(edit_line);
        let prev_count = self.doc.block_count();
        // 编辑前被编辑块的几何（kind/src_line/aux）——用于判定是否需要降级为结构编辑：
        // 代码/表格**内容行**的非换行键入块数/几何都变 → 走非结构路径（Patch + 局部
        // 作废整形缓存），免整篇 Pass1 高度重建 + fold_plan 重建 + block_regions 整表
        // 扫描（旧实现 `in_code_table` 一律 structural → 表内每键 ~140ms）。
        let edit_blk = self.doc.block_index_for_line(edit_line);
        let prev_geo = self
            .doc
            .block_meta(edit_blk)
            .map(|m| (m.kind, m.src_line, m.aux));
        // 编辑前被编辑块的内容指纹（整形缓存**内容寻址**键）：编辑后内容已变 → 剔除
        // 旧指纹行（否则旧条目滞留到被 LRU 淘汰；测试与内存卫生都要它即时消失）。
        let old_cid = frame::block_content_id(&self.doc, edit_blk);
        self.doc.edit(r.clone(), text);
        let cross_newline = text.contains('\n') || removed.contains('\n') || text.contains('\r');
        let breaks_fence = text.contains("```") || text.trim_start().starts_with('|');
        let count_same = self.doc.block_count() == prev_count;
        // 几何不变 = 新块 meta 与编辑前 (kind, src_line, aux) 逐项一致；不一致（块被
        // 吞并/分裂/变成表格/围栏等）必须结构重建。
        let geo_same = match (self.doc.block_meta(edit_blk), prev_geo) {
            (Some(nm), Some(om)) => nm.kind == om.0 && nm.src_line == om.1 && nm.aux == om.2,
            _ => false,
        };
        let structural = cross_newline || breaks_fence || !count_same || !geo_same;
        // Pass1 增量重估定位 + 整形/预热缓存作废（结构 vs 非结构两路，见 `invalidate_after_edit`）。
        self.invalidate_after_edit(
            edit_line, edit_blk, prev_count, structural, old_cid, &removed, text,
        );
        // —— 内容派生（derived）的增量判定（详见 `update_derived_after_edit`）——
        self.update_derived_after_edit(edit_line, &old_line, structural, text, &removed);

        self.caret = (r.start + inserted_len).min(self.len());
        self.sel_anchor = None;
        // 输入/删除/粘贴 → 静止期常亮（blink 暂停，先亮住再继续闪）。
        self.kick_caret();
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        crate::prof::end("wy.replace_range", _t_rr);
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    /// B2 撤销单元 + 事务合并（无定时器确定性判定，同 `editor/undo.rs`）：
    /// 「同意图且邻接」→ 并入当前打开的单元（净 `insert`/`delete` 累积成一次替换），
    /// 否则关闭上一个单元并入栈并开新单元；200 深可覆盖远多于 200 字符的内容。
    /// 返回是否并入上一事务。
    fn record_undo_unit(&mut self, r: &Range<usize>, removed: &str, text: &str) -> bool {
        let inserted_len = text.len();
        let intent = if removed.is_empty() && !text.is_empty() && !text.contains('\n') {
            EditKind::Typing
        } else if !removed.is_empty() && text.is_empty() {
            // 纯删除：退格（r.end == 光标）vs 正向删除（r.start == 光标）。
            if self.caret == r.end {
                EditKind::Backspace
            } else {
                EditKind::Delete
            }
        } else {
            EditKind::Atomic
        };
        // I1 时间窗合并：与上一编辑停顿 ≥ UNDO_GROUP_MS（300ms）→ 视为新事务（持续打字
        // 合并、停顿打断，同 editor/undo.rs）。std 单调钟纯时间判定，无定时器副作用。
        let within_window = self.last_edit_at.is_none_or(|t| t.elapsed() < UNDO_GROUP_MS);
        let merges = within_window
            && match self.last_edit {
                Some((k, s, _e)) if k == intent => match intent {
                    // 纯插入区间零宽：上一编辑的**插入终点** = 其起点 + 插入长。
                    EditKind::Typing => r.start == s + inserted_len,
                    EditKind::Backspace => r.end == s, // 向后退格首尾相接
                    EditKind::Delete => r.start == s,  // 同起点延伸
                    EditKind::Atomic => false,
                },
                _ => false,
            };
        if merges {
            // 并入打开单元：净区间/文本按意图累积（逆操作坐标不变形，见 UndoUnit）。
            let u = self.open_unit.as_mut().expect("能合并必已有打开的单元");
            u.insert.push_str(text);
            match intent {
                EditKind::Backspace => {
                    // 新增删除在左侧：`removed` 前插、区间起点左移（退格序列自右向左）。
                    u.delete = format!("{removed}{}", u.delete);
                    u.range.start = u.range.start.saturating_sub(removed.len());
                }
                // 同点删除延伸：删除文本按序追加。
                EditKind::Delete => u.delete.push_str(removed),
                _ => {} // 纯键入：insert 已追加，delete 保持（纯键入时为空）。
            }
            u.range.end = u.range.start + u.insert.len();
        } else {
            // 关闭上一个打开的单元（撤销单元自此固化）并入栈，开新单元。
            if let Some(prev) = self.open_unit.take() {
                self.push_undo_unit(prev);
            }
            self.open_unit = Some(UndoUnit {
                range: r.start..(r.start + inserted_len),
                insert: text.to_string(),
                delete: removed.to_string(),
                caret_before: self.caret,
                forward_caret: 0, // undo 时再写
            });
        }
        self.redo_stack.clear();
        self.last_edit = Some((intent, r.start, r.start + inserted_len));
        self.last_edit_at = Some(std::time::Instant::now());
        merges
    }

    /// 入撤销栈（深度上限 [`UNDO_LIMIT`]，超出丢最旧）。
    fn push_undo_unit(&mut self, u: UndoUnit) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(u);
    }

    /// Pass1 增量重估定位 + 整形/预热缓存作废：编辑行的**前一内容行**之后的块全部未变
    /// （前缀高度复用，不整篇重估）；结构编辑保留 < edit_blk 前缀的整形/重试缓存并归零
    /// 预热，非结构编辑只作废受影响块（块下标稳定）。
    #[allow(clippy::too_many_arguments)] // 编辑管线的正交输入
    fn invalidate_after_edit(
        &mut self,
        edit_line: usize,
        edit_blk: usize,
        prev_count: usize,
        structural: bool,
        old_cid: u64,
        removed: &str,
        text: &str,
    ) {
        // 每轮编辑先清「结构编辑待消费」标志；结构性分支再置位（见字段注释）。
        self.struct_edit_pending.set(false);
        // 内容寻址整形缓存：被编辑块旧指纹行先剔除（内容已变，本帧起按新指纹重排）。
        self.shape_rows.borrow_mut().invalidate_ids(&[old_cid]);
        // Pass1 增量高度重估起点；三段式（前缀 / 变化中段 / 尾部复用）定位用。
        self.struct_dirty_line.set(edit_line);
        self.struct_old_end_line
            .set(edit_line + removed.matches('\n').count());
        self.struct_line_delta
            .set(text.matches('\n').count() as i64 - removed.matches('\n').count() as i64);
        if structural {
            self.struct_ver.set(self.struct_ver.get().wrapping_add(1));
            *self.pending_patch_blocks.borrow_mut() = None;
            // 结构编辑：块行号/身份可能位移 → 数学位图钉随行号作废。
            self.clear_math_pins();
            // 结构编辑：被替换/插入**行窗**内的 src_line 旧键会误套到同线新块（把旧
            // 代码块的横向滚动/缩放残留到新块）→ 按窗清块视图态；窗外块内容未变保留。
            self.clear_src_line_view_state(
                edit_line,
                removed.matches('\n').count().max(text.matches('\n').count()),
            );
            // fold_plan deltas 增量平移的编辑信息：被编辑块旧下标 + 块数净增。
            self.struct_blk_old.set(edit_blk);
            self.struct_blk_span_delta
                .set(self.doc.block_count() as i64 - prev_count as i64);
            self.struct_edit_pending.set(true);
            // 整形缓存**内容寻址**（`frame::block_content_id`）：位移但内容未变的块指纹
            // 不变 → Pass2 自动命中，无需 retain_prefix；只有内容真正变化的块（编辑块，
            // 指纹已变）自然 miss。（旧实现按块下标键，这里曾不得不 retain_prefix 保前缀
            // 丢后缀，Enter 仍整带重排。）notdef 重试表仍按下标：前缀保留、≥ edit 作废。
            let prefix_keep = edit_blk.min(prev_count);
            self.notdef_retry.borrow_mut().retain_prefix(prefix_keep);
            // 结构编辑清空整形缓存 → 预热进度全部作废：归零方向/偏差/带序号，
            // 下一帧由带序号跨界重新判定方向（否则拿旧 dir 在空区间空转）。
            // 静置门控签名一并复位（内容已变，下一 prewarm 帧按「变化帧」放行）。
            let mut pw = self.prewarm.borrow_mut();
            pw.dir = 0;
            pw.bias = 0;
            pw.last_band_idx = None;
            pw.idle = 0;
            pw.sig = None;
        } else {
            // 非结构性：块下标稳定，记受影响的编辑行块。
            let blk = self.doc.block_index_for_line(edit_line);
            *self.pending_patch_blocks.borrow_mut() = Some(vec![blk]);
            // 该块内容变了 → 指纹自然变 → 整形缓存自生效（无需显式作废）。
            self.notdef_retry.borrow_mut().clear_block(blk);
        }
    }

    /// 内容派生（derived）的增量判定（详见 `region_line_sig`）：更新 math/alert 标记计数；
    /// 结构编辑把派生区域**行号平移**（B3 增量）并重建 block_regions；非结构编辑比对行
    /// 签名，签名不变即区域结构不变 → 走数学块 source 小修补（都不全篇重建）。
    /// 目标：**纯文本键入**（非结构、行签名不变、不在数学块内）不动 `regions_ver`
    /// → `derived` Rc 跨键复用——大文档（尤其结构密集）「输入卡死」的根因是旧实现
    /// 每键对全篇 `math_regions`×2 + `alert_*`×2 + 行内逐行扫 ≈1s。
    /// （公式光栅已走「可见带懒加载」，不再需要另一路全篇数学重扫信号。）
    /// M5-B3-1 子项 3：**签名变化但可精确证明区域结构未变**的行（普通行↔普通行，行内
    /// `$` 有无翻转）不再整篇重建（见 [`Self::maybe_patch_sig_changed_plain`]）。
    /// 子项 4：结构编辑纯插入且未新建命中折叠键标题 → 行号平移（含标题折叠体），
    /// **跨行删除/合并** 回退整篇重建。
    fn update_derived_after_edit(
        &mut self,
        edit_line: usize,
        old_line: &str,
        structural: bool,
        text: &str,
        removed: &str,
    ) {
        self.math_dollar_cnt.set(
            self.math_dollar_cnt.get() + text.bytes().filter(|&b| b == b'$').count()
                - removed.bytes().filter(|&b| b == b'$').count(),
        );
        self.alert_marker_cnt.set(
            self.alert_marker_cnt.get() + text.matches("[!").count()
                - removed.matches("[!").count(),
        );
        if structural {
            // 行索引 / 块结构变化：派生区域**行号平移**——编辑线之上的区域不变、之下
            // 平移 Δ；有区域跨线 / 派生未缓存才回退整篇重建。可编辑块区域（代码/表格）
            // 下标随之变化 → 一并重建（否则导航判定错位）。
            let line_delta =
                (text.matches('\n').count() as i64) - (removed.matches('\n').count() as i64);
            let removed_nl = removed.matches('\n').count();
            if !self.shift_derived_after_edit(edit_line, line_delta, removed_nl) {
                self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
            } else if self.edit_window_creates_folded_heading(
                edit_line,
                text.matches('\n').count(),
            ) || self.edit_window_creates_region_marker(
                edit_line,
                text.matches('\n').count(),
            ) {
                // M5-B3-1 子项 4：**插入窗口新建了命中已折叠键的标题行**（重复折叠
                // 同款标题：行号平移只能搬旧折叠体、无法新建折叠体）→ 回退整篇重建。
                // Cut-1a 补口：**新建区域起始行**（`$$` 围栏 / 整行 `$$…$$` / `> [!]`
                // 警报）同理——平移搬不动「还没建出来」的区域（见
                // `edit_window_creates_region_marker`）。
                // （Cut-1a：跨行删除/合并的边界语义已由 `shift_derived_after_edit` 的
                // 显式区域边界判定承担——纯文本合并不再无谓整建。）
                self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
            } else {
                // 成功平移：region 结构不变、只是行号动 → 递增 `fp_sets` 缓存键的
                // 行号平移分量（集合命中旧行号即错）。
                self.regions_shift_ver
                    .set(self.regions_shift_ver.get().wrapping_add(1));
            }
            // B4：`block_regions` 增量更新（编辑线附近重算 + 其余平移），不再每键
            // O(块数) 整表扫描（6MB/10 万块 debug ~5.9ms/键）。
            self.update_block_regions_after_edit(edit_line, line_delta);
        } else {
            // 非结构性：代码/表格区域布局不变 → `block_regions` 复用，跳过
            // `rebuild_block_regions` 的 O(块数) 整表扫描（6MB/6 万块 debug ~2.3ms/键，
            // 是「输入"仍卡"」里仅次于整串复制的第二大头；`structural` 判定已保证
            // 非结构性编辑既不跨行、也不在代码/表格块内 → 区域必然不变）。
            let new_line = self.doc.line_str(edit_line);
            if region_line_sig(old_line) != region_line_sig(&new_line) {
                // 该行 region 签名变了（出现/消失 `$`、``` 、`[!` 或 `$$` 边界等）。
                // M5-B3-1 子项 3：普通行 ↔ 普通行（行内 `$` 有无翻转）可精确判定区域
                // 结构未变 → 只补受影响块 source；其余签名变化仍整篇重建。
                if self.maybe_patch_sig_changed_plain(edit_line, old_line, &new_line) {
                    // 本地已修补，`regions_ver` 不递增（derived Rc 跨键复用）。
                } else {
                    self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
                }
            } else {
                let _t_m = crate::prof::begin();
                // 快路径：区域结构不变，只做受影响点的小修补（都不全篇重建）——
                // 编辑行在 `$$…$$` 数学块内 → 重算该块 `source`（卡片 / 排版键正确，
                // 否则离开块后卡片显示旧公式）。
                self.patch_math_block_source(edit_line, &new_line);
                crate::prof::end("wy.rr.math", _t_m);
            }
        }
    }

    /// M5-B3-1 子项 3：非结构编辑、行签名变化，但可**精确判定**区域结构未变——
    /// 仅当编辑行「普通行 ↔ 普通行」（`is_plain_region_line`，class 7，任意行内 `$`
    /// 有无翻转）：`math_regions` 只认 `$$` 围栏行与整行 `$$…$$`，行内 `$` 由可见带
    /// 懒光栅 claim、不构成区域；`alert_*` 只认 `>`/`[!]` 前缀。该类编辑唯一可能影响
    /// 的区域是「恰为该 `$$…$$` 块 body 行」时的 source 内容 → 就地补块即可。
    /// 其余签名变化（围栏/`$$` 边界/警报/对齐标记出入）→ `false` 回退整篇重建。
    ///
    /// 权衡：本分支是「只加速常见路径、稀有路径保全量」——`$` 在普通文本行打出行内
    /// 公式是最常见触发面；`$$` 围栏出入/警报/代码围栏边界仍整篇重建（正确 > 速度；
    /// 增量开块本可前向扫到闭合围栏但风险高于收益，暂不）。
    fn maybe_patch_sig_changed_plain(
        &mut self,
        edit_line: usize,
        old_line: &str,
        new_line: &str,
    ) -> bool {
        // 只对「普通行 ↔ 普通行」做增量：任一侧是围栏/`$$` 边界/警报/对齐标记 →
        // 区域结构可能真的变 → 全篇重建。
        if !(is_plain_region_line(old_line) && is_plain_region_line(new_line)) {
            return false;
        }
        let derived = self.derived();
        let d = derived.borrow();
        let Some(idx) =
            super::view::region_containing(&d.math_regions, |r| r.range.clone(), edit_line, true)
        else {
            // 不在任何数学块内：行内 `$` 翻转不构成/不改变任何区域。
            return true;
        };
        let rng = d.math_regions[idx].range.clone();
        drop(d);
        // 单行 `$$x^2$$` / 开界或闭界围栏行本身 → 该块结构受直接影响 → 不能增量。
        if rng.end == rng.start + 1 || edit_line == rng.start || edit_line == rng.end - 1 {
            return false;
        }
        // 普通 body 行内的 `$` 翻转：块区间不变，只重拼该块 source。
        self.patch_math_block_source_at(edit_line, idx, new_line);
        true
    }

    /// 编辑行在 `$$…$$` 数学块内 → 就地重算该块 `source`（卡片 / 排版键正确，否则离开
    /// 块后卡片显示旧公式）。非结构编辑的两个走快路径的分支（签名不变、签名变但仅普通
    /// 行美元翻转）共用；都**不**整篇重建 derived。
    fn patch_math_block_source(&mut self, edit_line: usize, new_line: &str) {
        let derived = self.derived();
        let d = derived.borrow();
        if let Some(idx) =
            super::view::region_containing(&d.math_regions, |r| r.range.clone(), edit_line, true)
        {
            let rng = d.math_regions[idx].range.clone();
            drop(d);
            self.patch_math_block_source_impl(edit_line, idx, new_line, &rng);
        }
    }
    /// 已知块下标版的 source 修补（`maybe_patch_sig_changed_plain` 已定位 idx，省一次二分）。
    fn patch_math_block_source_at(&mut self, edit_line: usize, idx: usize, new_line: &str) {
        let derived = self.derived();
        let d = derived.borrow();
        let rng = d.math_regions[idx].range.clone();
        drop(d);
        self.patch_math_block_source_impl(edit_line, idx, new_line, &rng);
    }
    fn patch_math_block_source_impl(
        &self,
        _edit_line: usize,
        idx: usize,
        new_line: &str,
        rng: &std::ops::Range<usize>,
    ) {
        let derived = self.derived();
        let mut d2 = derived.borrow_mut();
        let old_src = d2.math_regions[idx].source.clone();
        let new_source = if rng.end == rng.start + 1 {
            // 单行 `$$x^2$$`：source = 行内内层。
            inline_style::one_line_math(new_line)
                .map(str::to_string)
                .unwrap_or(old_src)
        } else if rng.start + 1 < rng.end.saturating_sub(1) {
            let hi = rng.end.saturating_sub(1);
            (rng.start + 1..hi)
                .map(|l| self.doc.line_str(l))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        if d2.math_regions[idx].source != new_source {
            d2.math_regions[idx].source = new_source;
        }
    }

    /// M5-B3-1 子项 4 守卫：结构编辑**插入窗口**（`edit_line..=edit_line + 插入换行数`）
    /// 内出现了「`heading_fold_regions` 会认作标题且命中已折叠键」的新行——行号平移只能
    /// 搬旧折叠体、无法新建折叠体（同键标题多处重复折叠）→ 返回 `true` 让调用方回退
    /// 整篇重建。只在有折叠态时干活（O(插入行数) 次 line_str + HashSet 查，单键 Enter ≈
    /// 常数）；行号平移本身已在 `shift_derived_after_edit` 处理「编辑线落在体**内**」。
    fn edit_window_creates_folded_heading(&self, edit_line: usize, inserted_nl: usize) -> bool {
        let folded = self.folded_headings.borrow();
        if folded.is_empty() {
            return false;
        }
        let max_line = self.doc.line_starts().len().saturating_sub(1);
        for l in edit_line..=(edit_line + inserted_nl).min(max_line) {
            let raw = self.doc.line_str(l);
            if inline_style::line_heading_level(&raw).is_some() && folded.contains(raw.trim()) {
                return true;
            }
        }
        false
    }

    /// 记录撤销快照并做一次文本替换（任务框 toggle / 警报折叠共用）。
    /// 快照与重做栈的维护在 [`Self::replace_range`] 内统一完成。
    /// Cut-1a 补口：结构编辑**插入窗口**（`edit_line..=edit_line + 插入换行数`）内出现了
    /// **新建的区域起始行**（`math_regions`/`alert_regions` 会认作开界的 `$$` 围栏 / 整行
    /// `$$…$$` / `> [!]` 警报）——行号平移只能搬旧区域、无法**新建**区域，纯平移会在
    /// 派生里留下陈旧空表（例：纯文本文档粘贴/合并出一个 `$$` 围栏行，`shift_derived_after_edit`
    /// 对空缓存返回 true，`derived()` 命中旧 `regions_ver` 的空 Rc → 数学块隐形）。
    /// 返回 `true` 让调用方回退整篇重建。先计数短路（无 `$`/`[!` 必无此类区域），
    /// O(插入行数) 次 `line_str`——纯文本合并窗口内无任何区域开界 → 不影响 Cut-1a 快路径。
    fn edit_window_creates_region_marker(&self, edit_line: usize, inserted_nl: usize) -> bool {
        if self.math_dollar_cnt.get() == 0 && self.alert_marker_cnt.get() == 0 {
            return false;
        }
        let max_line = self.doc.line_starts().len().saturating_sub(1);
        for l in edit_line..=(edit_line + inserted_nl).min(max_line) {
            let raw = self.doc.line_str(l);
            if raw.trim() == "$$" || inline_style::one_line_math(&raw).is_some() {
                return true;
            }
            if let Some(p) = inline_style::blockquote_prefix(&raw)
                && inline_style::alert_prefix(&raw[p..]).is_some()
            {
                return true;
            }
        }
        false
    }

    pub(crate) fn apply_edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.replace_range(range, text, cx);
    }

    /// 翻转第 `line` 行（`> [!NOTE]-` / `+`）的折叠符（C3）：`-`→`+` 展开、`+`→`-` 折叠。
    pub(crate) fn toggle_alert_fold_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let raw = self.doc.slice_str(start..end);
        if let Some((at, cur)) = recognition::alert_fold_char(&raw) {
            let range = start + at..start + at + 1;
            let repl = if cur { "+" } else { "-" };
            if range.end <= end {
                self.apply_edit(range, repl, cx);
            }
        }
    }

    /// 翻转第 `line` 行标题的折叠态（视图本地，不写源码）：`## Goals` body 行折叠。
    /// 折叠时若光标在该 section 的 body 内会被 reveal 弹开（折叠无效），先把光标
    /// 放到标题行末尾（与 live 的 `move_to(end)` 同款）。
    pub(crate) fn toggle_heading_fold_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let key = self.doc.slice_str(start..end).trim().to_string();
        if key.is_empty() {
            return;
        }
        let was_folded = self.folded_headings.borrow().contains(&key);
        if !was_folded {
            // 折叠前：光标若在 section body 内，先移到标题行末（否则 body 有光标被 reveal）。
            let single = std::collections::HashSet::from([key.clone()]);
            let crow = self.row_col(self.caret()).0;
            if inline_style::heading_fold_regions(&self.doc.text_shared(), &single)
                .iter()
                .any(|r| crow > r.start && crow < r.end)
            {
                self.set_caret(end);
            }
        }
        {
            let mut folded = self.folded_headings.borrow_mut();
            if was_folded {
                folded.remove(&key);
            } else {
                folded.insert(key);
            }
        }
        // 折叠态变化 → `fold_plan` 缓存失效（滚动/带移动间不重算，但折叠变必须重建）。
        self.fold_ver.set(self.fold_ver.get().wrapping_add(1));
        // M5-B3-1 子项 4：标题折叠体列表是 `fold_plan` sets 输入之一、按 `fold_ver`
        // 进缓存键——这里**就地**重扫进 `derived`（`regions_ver` 键不变 → 不整篇重建；
        // 折叠翻转本身稀有，允许一次 O(n) 全扫 + `text_shared`；逐键输入/Enter 不再触达）。
        if let Some((_, rc)) = &*self.derived.borrow() {
            let mut d = rc.borrow_mut();
            let folded = self.folded_headings.borrow();
            d.heading_fold_bodies = if folded.is_empty() {
                Vec::new()
            } else {
                inline_style::heading_fold_regions(&self.doc.text_shared(), &folded)
            };
        }
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 翻转第 `line` 行任务框（C1）：`[ ]`↔`[x]`。
    pub(crate) fn toggle_checkbox_at_line(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_end(line).max(start);
        let raw = self.doc.slice_str(start..end);
        if let Some(new_line) = inline_style::toggle_task_checkbox(&raw) {
            self.apply_edit(start..end, &new_line, cx);
        }
    }
    /// 撤销（D1）：把当前打开的撤销单元固化入栈，弹出上一单元并沿增量管道打回其
    /// **净逆操作**（不再整体重建模型）；被撤销的单元（带撤销瞬间光标）进重做栈。
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("undo");
        if let Some(u) = self.open_unit.take() {
            self.push_undo_unit(u);
        }
        if let Some(mut u) = self.undo_stack.pop() {
            // 重做恢复位定格为「按下撤销瞬间」的光标（同 `editor/undo.rs`）。
            u.forward_caret = self.caret;
            self.redo_stack.push(u.clone());
            let target = u.caret_before;
            self.apply_raw_edit(u.range.clone(), &u.delete, target, cx);
        }
        self.last_edit = None;
        self.last_edit_at = None;
    }

    /// 重做（D1）。
    pub fn redo(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("redo");
        if let Some(u) = self.open_unit.take() {
            self.push_undo_unit(u);
        }
        if let Some(u) = self.redo_stack.pop() {
            self.undo_stack.push(u.clone());
            let target = u.forward_caret;
            // 前向：当前文档在 `[range.start, range.start+delete.len())` 处是 `delete`，
            // 用 `insert` 换回（撤销的逆）。
            let r = u.range.start..(u.range.start + u.delete.len());
            self.apply_raw_edit(r, &u.insert, target, cx);
        }
        self.last_edit = None;
        self.last_edit_at = None;
    }

    /// A3：只读右栏预览的**增量桥**——把左栏源码编辑区同一次增删（`CodeEditor::take_last_edit`
    /// 的 `(start, old_end, head)` 轨迹）重放过来。复用 undo/redo 的增量管线
    /// `apply_raw_edit`（`Doc::edit` 增量块扫描 + Pass1 三段式 + derived 小修补 + 单块整形
    /// 作废），**不写撤销栈**（预览不参与撤销）、光标保持原位（只读光标不可见勿动）、
    /// **绕过 `editable` 门控**（桥是宿主同步写，非用户编辑；用户的一切编辑入口仍被 A1
    /// 短路）。同步保持：装配源与编辑区同源，此后逐键重放 → 预览恒等于编辑区文本。
    pub fn preview_apply_edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let cur = self.caret;
        self.apply_raw_edit(r, text, cur, cx);
    }

    /// 撤销/重做共用增量编辑：与 `replace_range` 完全同款（`Doc::edit` 增量块扫描 +
    /// Pass1 三段式重估 + derived 小修补 + 单块整形作废 + 结构判定），**唯二区别**：
    /// 不写撤销单元（本调用由 undo/redo 发起）、光标按目标位放置。UndoUnit 的
    /// `range`/`insert`/`delete` 组合即自带精确坐标，直接复用这套增量管线——
    /// 大文档撤销从「整体重建 6.4MB」降到「一次局部增量编辑」。
    fn apply_raw_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        target_caret: usize,
        cx: &mut Context<Self>,
    ) {
        let len = self.len();
        let r = range.start.min(len)..range.end.min(len);
        if r.start == r.end && text.is_empty() {
            return;
        }
        let _t_rr = crate::prof::begin();
        // —— 编辑日志（方案2）：撤销/重做（与 `preview_apply_edit` 的右栏桥）共用本
        // 增量管道。撤销/重做也经此缴存 op → live 会话编辑日志按序重放即得最终文本。
        // 右栏 `md_wy` 未开启收集，其逐 op 桥重放（`preview_apply_edit`）不会误入日志。
        if let Some(log) = &mut self.live_ops {
            log.push((r.clone(), SharedString::from(text)));
        }
        let removed = self.doc.slice_str(r.clone());
        let edit_line = self.row_col(r.start).0;
        let old_line = self.doc.line_str(edit_line);
        let prev_count = self.doc.block_count();
        let edit_blk = self.doc.block_index_for_line(edit_line);
        let prev_geo = self
            .doc
            .block_meta(edit_blk)
            .map(|m| (m.kind, m.src_line, m.aux));
        // 与 `replace_range` 同：剔除被编辑块的旧内容指纹行（内容寻址缓存）。
        let old_cid = frame::block_content_id(&self.doc, edit_blk);
        self.doc.edit(r.clone(), text);
        let cross_newline = text.contains('\n') || removed.contains('\n') || text.contains('\r');
        let breaks_fence = text.contains("```") || text.trim_start().starts_with('|');
        let count_same = self.doc.block_count() == prev_count;
        let geo_same = match (self.doc.block_meta(edit_blk), prev_geo) {
            (Some(nm), Some(om)) => nm.kind == om.0 && nm.src_line == om.1 && nm.aux == om.2,
            _ => false,
        };
        let structural = cross_newline || breaks_fence || !count_same || !geo_same;
        self.invalidate_after_edit(
            edit_line, edit_blk, prev_count, structural, old_cid, &removed, text,
        );
        self.update_derived_after_edit(edit_line, &old_line, structural, text, &removed);
        self.caret = target_caret.min(self.len());
        self.sel_anchor = None;
        self.marked = None;
        // 撤销/重做等净逆操作落地 → 静止期常亮。
        self.kick_caret();
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        crate::prof::end("wy.replace_range", _t_rr);
        cx.emit(WyEvent::Changed);
        cx.notify();
    }

    pub(crate) fn backspace(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        // 选中的图片对象：Backspace 删除整张图片行（含换行；记撤销栈）。
        if let Some(line) = self.selected_image.get() {
            self.delete_selected_image(line, cx);
            return;
        }
        // 选中的**行内**图片：Backspace 删除整段 `![…](…)`（保留行内其它文本；记撤销栈）。
        if let Some(refd) = self.selected_inline_image.get() {
            self.delete_selected_inline_image(refd, cx);
            return;
        }
        crate::prof::op_start("backspace");
        // 有选区 → 直接删选区（等价退格删除选中文字）。
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let at = self.caret.min(self.len());
        if at == 0 {
            return;
        }
        // 围栏/单元格边界守卫：退格不得删到代码块首内容行行首（会吃掉 ``` 围栏的
        // `\n`）或表格单元格内容起点（会吃掉 `| `）。
        let (bl, bcol) = self.row_col(at);
        if self.backspace_blocked(bl, bcol) {
            return;
        }
        // 上一字符起点（rope char boundary 回溯，O(chars)，不复制前缀整串）。
        let prev = self.doc.rope().floor_char_boundary(at - 1);
        self.replace_range(prev..at, "", cx);
    }

    /// 退格是否被边界守卫拦截（代码首内容行行首 / 表格单元格内容起点 / 图片 alt 起点）。
    fn backspace_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { lo, .. }) => line == lo && col == 0,
            Some(BlockRegion::Table { .. }) => self
                .table_cell_bounds(line, col)
                .is_some_and(|(s, _)| col <= s),
            None => {
                // 图片行 alt：起点即 `![` 边界，退格不得吃到图片标记（行会变非图片）。
                self.image_alt_segment(line)
                    .is_some_and(|(lo, _)| col <= lo)
            }
        }
    }

    pub(crate) fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        // 选中的图片对象：Delete 删除整张图片行（含换行；记撤销栈）。
        if let Some(line) = self.selected_image.get() {
            self.delete_selected_image(line, cx);
            return;
        }
        // 选中的**行内**图片：Delete 删除整段 `![…](…)`（保留行内其它文本；记撤销栈）。
        if let Some(refd) = self.selected_inline_image.get() {
            self.delete_selected_inline_image(refd, cx);
            return;
        }
        crate::prof::op_start("delete");
        if let Some(range) = self.selection() {
            return self.replace_range(range, "", cx);
        }
        let at = self.caret.min(self.len());
        if at >= self.len() {
            return;
        }
        // 围栏/单元格边界守卫：删除不得删到代码块末内容行行尾（会把闭合围栏并进
        // 内容行）或表格单元格内容尾（会吃掉 `|`）。
        let (dl, dcol) = self.row_col(at);
        if self.delete_blocked(dl, dcol) {
            return;
        }
        // 下一字符尾（rope char boundary，O(chars)）。
        let end = self.doc.rope().ceil_char_boundary(at + 1);
        self.replace_range(at..end, "", cx);
    }

    /// 删除是否被边界守卫拦截（代码末内容行行尾 / 表格单元格内容尾 / 图片 alt 尾）。
    fn delete_blocked(&self, line: usize, col: usize) -> bool {
        match self.region_content(line) {
            Some(BlockRegion::Code { hi, .. }) => {
                let len = self.line_end(line) - self.line_start(line);
                line == hi && col >= len
            }
            Some(BlockRegion::Table { .. }) => self
                .table_cell_bounds(line, col)
                .is_some_and(|(_, e)| col >= e),
            None => {
                // 图片行 alt：尾即 `](` 边界，删除不得吃到闭合中括号（行会变非图片）。
                self.image_alt_segment(line)
                    .is_some_and(|(_, hi)| col >= hi)
            }
        }
    }

    pub(crate) fn newline(&mut self, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("newline");
        // 表格内换行会拆坏表结构（MVP 先 no-op）；代码块内换行合法（分行）。
        // 图片行 alt（`![` 之后到 `](` 之前）内换行会拆坏图片语法 → 同样 no-op
        // （光标在 alt 内或未越过 alt 尾；越过整行尾（`)(src)` 之后）才放行）。
        let (nl, col) = self.row_col(self.caret);
        if matches!(self.region_content(nl), Some(BlockRegion::Table { .. }))
            || self
                .image_alt_segment(nl)
                .is_some_and(|(_lo, hi)| col <= hi)
        {
            return;
        }
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "\n", cx);
    }

    /* ---------- 光标移动 ---------- */

    pub(crate) fn move_left(&mut self) {
        let (line, col) = self.row_col(self.caret);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if col > lo {
                // 段内退一字符（rope char boundary，不复制前缀整串）。
                self.caret = self
                    .doc
                    .rope()
                    .floor_char_boundary(self.caret.saturating_sub(1))
                    .max(seg_start);
            } else if self.caret >= seg_start && self.caret < seg_end {
                // 段首（光标坐在标记后/代码内容行行首）→ 回上一**可视**行段尾
                // （跳过围栏行，光标绝不落进只读围栏）。
                self.caret = if line > 0 {
                    let p = self.prev_render_line(line - 1);
                    let e = self.line_end(p);
                    self.editable_segment(p)
                        .map_or(self.line_start(p), |(_l, h)| self.line_start(p) + h)
                        .min(e)
                        .max(self.line_start(p))
                } else {
                    0
                };
            }
        } else if line > 0 {
            let p = self.prev_render_line(line - 1);
            self.caret = self.line_end(p).max(self.line_start(p));
        } else if self.caret > 0 {
            self.caret -= 1;
        }
        self.moved();
    }

    pub(crate) fn move_right(&mut self) {
        let (line, _col) = self.row_col(self.caret);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = self.line_start(line) + lo;
            let seg_end = self.line_start(line) + hi;
            if self.caret < seg_end {
                let n = self.doc.rope().ceil_char_boundary(self.caret + 1);
                self.caret = n.min(seg_end).max(seg_start + 1);
            } else {
                // 段尾 → 下一**可视**行段首（跳过围栏行）。
                self.caret = self.next_segment_start(line);
            }
        } else {
            self.caret = self.next_segment_start(line);
        }
        self.moved();
    }

    fn next_segment_start(&self, line: usize) -> usize {
        let total = self.doc.line_starts().len();
        if line + 1 < total {
            // 下一行是围栏（代码块边界）→ 跳到围栏之后的内容/段落行段首。
            let n = if self.is_fence_line(line + 1) {
                self.next_render_line(line + 1)
            } else {
                line + 1
            };
            self.editable_segment(n)
                .map_or(self.line_end(n), |(lo, _)| self.line_start(n) + lo)
        } else {
            self.len()
        }
    }

    pub(crate) fn move_up(&mut self) {
        let (line, _) = self.row_col(self.caret);
        self.caret = if line > 0 {
            self.place_in_line(self.up_target(line), self.visual_col())
        } else {
            0
        };
        self.moved();
    }

    /// 光标向上移动的落点行（块内逐内容行；跨围栏/块边进出）。
    fn up_target(&self, line: usize) -> usize {
        match self.region_content(line) {
            // 代码/表格内容行内部：上一内容行；已是块首内容行 → 跳出到上一可视行。
            Some(BlockRegion::Code { lo, .. }) if line > lo => line - 1,
            Some(BlockRegion::Table { dlo, .. }) if line > dlo => line - 1,
            Some(_) | None => self.prev_render_line(line - 1),
        }
    }

    pub(crate) fn move_down(&mut self) {
        let (line, _) = self.row_col(self.caret);
        let total = self.doc.line_starts().len();
        self.caret = if line + 1 < total {
            self.place_in_line(self.down_target(line), self.visual_col())
        } else {
            self.len()
        };
        self.moved();
    }

    /// 光标向下移动的落点行（块内逐内容行；跨围栏/块边进出）。
    fn down_target(&self, line: usize) -> usize {
        match self.region_content(line) {
            // 代码/表格内容行内部：下一内容行；已是块末内容行 → 跳出到下一可视行。
            Some(BlockRegion::Code { hi, .. }) if line < hi => line + 1,
            // 表头 → 跳过分隔行，进入首数据行。
            Some(BlockRegion::Table { header, .. }) if line == header => {
                self.next_render_line(line + 1)
            }
            Some(BlockRegion::Table { dhi, .. }) if line < dhi => line + 1,
            Some(_) | None => {
                let nxt = line + 1;
                if self.is_fence_line(nxt) {
                    self.next_render_line(nxt)
                } else {
                    nxt
                }
            }
        }
    }

    /// 光标行内“可视列”（段内渲染列）——上下保持列。
    fn visual_col(&self) -> usize {
        let (line, col) = self.row_col(self.caret);
        if let Some((lo, _)) = self.editable_segment(line) {
            col.saturating_sub(lo)
        } else {
            0
        }
    }

    /// 目标行按“可视列”落位（限段）。
    fn place_in_line(&self, line: usize, col: usize) -> usize {
        let ls = self.line_start(line);
        let le = self.line_end(line);
        if let Some((lo, hi)) = self.editable_segment(line) {
            let seg_start = ls + lo;
            let seg_end = ls + hi;
            let cap = (seg_end.saturating_sub(seg_start + 1)).min(80);
            (seg_start + col.min(cap)).min(seg_end).min(le)
        } else {
            le.max(ls)
        }
    }

    fn moved(&mut self) {
        // 方向键移动 → 打断撤销合并组。
        self.last_edit = None;
        self.last_edit_at = None;
        // 方向键移动 → 静止期常亮（blink 暂停，先亮住再继续闪）。
        self.kick_caret();
        // 表格内容行内：光标钳到最近单元格内容区间（不落在管道/空白上，避免输入破坏表）。
        let (line, col) = self.row_col(self.caret);
        if let Some(c) = self.clamp_caret_table(line, col) {
            self.caret = c;
        }
        *self.frame.borrow_mut() = None;
    }
}
