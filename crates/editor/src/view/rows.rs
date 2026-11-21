//! 行级物化 / 语法分片补色。`build_rows` 行数据窗口构建、行样式覆盖（`row_*`）与
//! syntect 分片补色（`syntect_fill_deferred` / `fill_place`）。（光标可见性已下沉
//! `editor_core::caret`，本批 Pattern 1。）

// 行折叠类型较长 → 放行
#![allow(clippy::type_complexity)]

use super::{
    merge_highlights,
    render,
};

use std::{
    borrow::Cow,
    ops::Range,
    rc::Rc,
};

use gpui::{
    HighlightStyle,
    Hsla,
    Window,
};

use super::{
    CodeEditor,
    HlFill,
    RowContent,
    RowData,
    HL_FILL_BUDGET_ROWS,
    HL_LOOKBACK_LINES,
};

impl CodeEditor {

    /// 测试：对指定逻辑行走真实的 `build_rows` 取数路径，返回该行**最终样式段**
    /// （Markdown 滤镜或 syntect，选区合并前）。用于验证 Markdown WYSIWYG 接线
    /// （`md_mode → scan_window → build_rows → merge_highlights`）。
    #[cfg(test)]
    pub(crate) fn test_line_styles(
        &mut self,
        window: &mut Window,
        logical_row: usize,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        if logical_row >= self.row_count || logical_row >= self.buffer.line_count() {
            return Vec::new();
        }
        self.test_warm_highlight(logical_row, logical_row);
        self.build_rows(window, logical_row, logical_row, logical_row, false, false)
            .into_iter()
            .next()
            .map(|rd| rd.content.highlights.clone())
            .unwrap_or_default()
    }

    /// 测试：走真实 `build_rows` 路径拍一行窗口快照（文本 + 渲染消费的合并后样式
    /// 段），供「无编辑滚动多帧后渲染不变 / 编辑后缓存失效」回归断言（M5-B2-3）。
    #[cfg(test)]
    #[allow(clippy::type_complexity)] // 测试专用（文本 + 样式段）复合快照类型。
    pub(crate) fn test_build_rows_snapshot(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
    ) -> Vec<(String, Vec<(Range<usize>, HighlightStyle)>)> {
        self.test_warm_highlight(lo, hi);
        self.build_rows(window, lo, hi, lo, false, false)
            .into_iter()
            .map(|rd| (rd.content.text.to_string(), rd.highlights().to_vec()))
            .collect()
    }

    /// 测试：**不预热**直接走 `build_rows` 的取数路径（每行最终样式段）。
    /// 方案 A 后首屏（空缓存）同步上色、不再延后 + `request_animation_frame`，故在
    /// 帧外直调 `build_rows` 安全——用「新开文档首屏」验证**首帧即有着色**（大 .rs
    /// 打开不再先纯文本、后补色）。仅适用于「新开文档首屏」窗口（小缺口），勿用于
    /// 滚动大缺口窗口（那帧仍会延后 + RAF，帧外直调会 debug_assert 炸）。
    #[cfg(test)]
    #[allow(clippy::type_complexity)] // 测试专用（文本 + 样式段）复合快照类型。
    pub(crate) fn test_build_rows_first_frame(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
    ) -> Vec<(String, Vec<(Range<usize>, HighlightStyle)>)> {
        self.build_rows(window, lo, hi, lo, false, false)
            .into_iter()
            .map(|rd| (rd.content.text.to_string(), rd.highlights().to_vec()))
            .collect()
    }

    /// 测试：直接调 `build_rows` 前先预热高亮缓存（仅非 Markdown 的 syntect 路径需要）。
    /// 首屏（方案 A）虽已同步上色，但**滚动大缺口**帧仍走「延后上色」且在 `build_rows`
    /// 里 `request_animation_frame`（只能在真实帧内调用）——测试在帧外直调 build_rows 会
    /// debug_assert 炸。这里绕过高亮器把 pending/settle 契约**帧外安全**执行完（与
    /// `build_rows` 的快路径/延后路径同一组 API），之后 `build_rows` 自然命中缓存，不再
    /// 走延后分支。缓存已覆盖则幂等（命中/小扩展）。Markdown（`md_mode`）走
    /// `scan_window`，不经过该延后分支，无需预热。
    #[cfg(test)]
    fn test_warm_highlight(&mut self, lo: usize, hi: usize) {
        if self.md_mode {
            return;
        }
        self.highlighter.set_dirty(self.hl_dirty.take());
        let _ = self.highlighter.accept_deferred(lo, hi, HL_LOOKBACK_LINES);
        let _ = self.highlighter.settle_pending(&self.buffer);
        let _ = self
            .highlighter
            .highlight_window(&self.buffer, lo, hi, HL_LOOKBACK_LINES)
            .len();
    }

    /// 测试：行缓存命中计数 / 条目数（滚动窗口内断言缓存复用、编辑后失效）。
    #[cfg(test)]
    pub(crate) fn test_row_cache(&self) -> (usize, usize) {
        self.row_cache.stats()
    }

    /// 测试：当前语法高亮「分片补色」进度快照 `(窗口(lo_l,hi_l), 可见顶, 游标)`。
    #[cfg(test)]
    pub(crate) fn test_hl_fill(&self) -> Option<((usize, usize), usize, usize)> {
        let f = self.hl_fill;
        Some((f.window, f.vis_top_l, f.cursor))
    }

    /// 测试：逻辑行 `logical` 当前在语法缓存里的真颜色段数（`0` = 未补色/不在缓存）。
    #[cfg(test)]
    pub(crate) fn test_hl_row_runs(&self, logical: usize) -> usize {
        self.highlighter.test_row_runs(logical)
    }

    // -----------------------------------------------------------------
    // 行数据
    // -----------------------------------------------------------------

    /// 光标 x：按帧状态现算（真实字形宽度，CJK 双宽不再按等宽列近似）→ 光标停在
    /// 真实字符边界上（横向滚动交给外层容器，行内文本从 x=0 全宽绘制，这里只算文本
    /// 本地 x）。不入缓存——光标位置每帧都在变，且空行分支与普通分支值一致
    /// （空行 head==line_start==line_end → byte_col=0 → x=0）。
    fn row_caret(&self, window: &mut Window, row: usize, show_caret: bool) -> Option<f32> {
        if !show_caret {
            return None;
        }
        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let head = self.selection.head();
        if head < line_start || head > line_end {
            return None;
        }
        let byte_col = head - line_start;
        Some(
            self.row_layout(window, row)
                .map(|layout| layout.x_for_index(byte_col).as_f32())
                .unwrap_or_else(|| byte_col as f32 * self.char_width.as_f32().max(1.)),
        )
    }

    /// 折叠首行指示：按帧状态现叠（折叠动作不失效内容缓存）。`Some(x)` = 折叠首行的
    /// 「…」指示器 x（= 行文本整形宽；独立元素渲染，不进 StyledText，不污染行内测量）。
    fn row_folded(&self, window: &mut Window, row: usize) -> Option<f32> {
        if self.folds.fold_starting_at(row).is_some() {
            Some(
                self.row_layout(window, row)
                    .map(|layout| layout.width().as_f32())
                    .unwrap_or_else(|| {
                        self.buffer.line_text(row).chars().count() as f32
                            * self.char_width.as_f32().max(1.)
                    }),
            )
        } else {
            None
        }
    }

    /// 选区合并层（动态，不入缓存）：选区坍缩返回 `None`（直接用缓存的无选区合并
    /// 结果，滚动帧零分配零合并）；选区活动返回本帧动态合并——`merge_highlights`
    /// 输入用缓存的 raw runs，主题底色 `sel_bg` 用当前值。选区/IME 组合变化绝不被
    /// 旧缓存吞掉：选中底色永远按 **当前** 选区现算（见 `merge_highlights` 的非重叠
    /// 语义，选区与语法段被合并成连续段，run 总量不超订）。
    fn row_highlight_override(
        &self,
        row: usize,
        content: &RowContent,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        if self.selection.is_collapsed() {
            return None;
        }
        let line_start = self.buffer.line_start(row);
        let line_end = self.buffer.line_end(row);
        let sel = self.selection.range();
        let lo = sel.start.max(line_start);
        let hi = sel.end.min(line_end);
        let sel_range = if hi > lo {
            Some((lo - line_start)..(hi - line_start))
        } else {
            None
        };
        Some(merge_highlights(&content.hl_styles, sel_range, self.sel_bg))
    }

    /// 行数据：渲染窗口内（`[lo, hi]`，**视觉行**）每行。
    ///
    /// 高亮必须整窗一次批量解析再按行进 `runs`：逐行调用 `highlight_window(row,row)`
    /// 会让缓存每行都失效并各自重解析 16 行回看上下文 → 每帧 O(窗口×回看) 重复解析，
    /// 是滚轮滚动"卡顿/不顺滑"的主因之一（syntect 上下文解析不便宜）。
    pub(super) fn build_rows(
        &mut self,
        window: &mut Window,
        lo: usize,
        hi: usize,
        vis_top: usize,
        show_caret: bool,
        focused: bool,
    ) -> Vec<RowData> {
        if self.row_count == 0 {
            return Vec::new();
        }
        // 窗口与滚动同在**视觉行**空间：折叠后行窗口以视觉行为界。
        let vtotal = self.folds.visual_line_count().max(1);
        let last_v = vtotal - 1;
        let lo = lo.min(last_v);
        let hi = hi.min(last_v);
        if hi < lo {
            return Vec::new();
        }
        // 把窗口两端映射成逻辑行，供高亮整窗批量解析与行文本提取；`vis_top` 也映射成
        // 逻辑行——延后「分片补色」从**可见区顶部**起（`lo_l` 含上方过扫，先给不可见
        // 行上色没用）。
        let lo_l = self.folds.logical_row(lo);
        let hi_l = self.folds.logical_row(hi);
        let vis_top_l = self.folds.logical_row(vis_top.min(last_v));
        // WY_TRACE breakdown：整窗行数据构建段（含语法/滤镜高亮解析 + 行内样式合并）。
        let _t_br = crate::prof::begin();
        // 整窗一次批量解析，快照成 owned（`.to_vec()` 结束内部的 `&mut self`
        // 借用，否则后续行物化/缓存写与高亮窗口的借用冲突）。
        // 返回切片约定：第 0 项 = `lo_l` 行（两级缓存均统一对齐到请求窗口）。
        // Markdown 文档走「所见即所得」滤镜（记号淡化 + 正文富样式）；其余语言走
        // syntect 语法高亮（`(Range, Hsla)` → 仅色 `HighlightStyle`）。
        // `colored_range`：本帧**有真颜色**的逻辑行区间（`None` = 占位空样式帧）。
        // 行缓存写入口径按它逐行判定：真色行走缓存 probe/write；占位空色行
        // **不入缓存**（否则把空色冻结进缓存 → 颜色永久缺失）。
        let colored_range: Option<(usize, usize)>;
        let hl_lines: Vec<Vec<(Range<usize>, HighlightStyle)>> = if self.md_mode {
            colored_range = Some((lo_l, hi_l));
            self.md_decor
                .scan_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES)
                .to_vec()
        } else {
            // 把最近编辑的增量登记喂给高亮器（局部重解析 + 收敛早退），只取走一次。
            self.highlighter.set_dirty(self.hl_dirty.take());
            let buf_version = self.buffer.version();
            // 分片补色进行中：本帧继续逐 32 行往下补（不入 quick 同步整窗、不 settle 整窗
            // 重锚——两者都会在补色早期一次性同步解析整窗几百 ms，把「首帧先出纯文本」
            // 打回「一帧冻结空白」，见 `HlFill::active`）。
            let fill_active = self.hl_fill.active(buf_version, lo_l, hi_l);
            // 大文档拖滚动条：窗口未覆盖且需有界重锚时，本帧走「分片补色」——从可见区
            // 顶部起只解析 `HL_FILL_BUDGET_ROWS` 行真颜色（窗口其余行空样式），颜色
            // 逐帧往下长；滚动停住（相邻帧窗口一致）由 `settle_pending` 补一次有界
            // 重锚补齐整窗（含上方过扫）、清 pending。
            let prev_pending = self.highlighter.pending();
            let quick = self
                .highlighter
                .accept_deferred(lo_l, hi_l, HL_LOOKBACK_LINES)
                && !fill_active;
            let settle = !quick
                && prev_pending == Some((lo_l, hi_l, HL_LOOKBACK_LINES))
                && !fill_active;
            if quick || settle {
                if settle {
                    self.highlighter.settle_pending(&self.buffer);
                }
                // 整窗已覆盖：清分片进度（本帧真颜色全窗，之后走缓存命中快路径）。
                self.hl_fill = HlFill::default();
                colored_range = Some((lo_l, hi_l));
                self.highlighter
                    .highlight_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES)
                    .to_vec()
                    .into_iter()
                    .map(|runs| {
                        runs.into_iter()
                            .map(|(r, c)| {
                                (
                                    r,
                                    HighlightStyle {
                                        color: Some(c),
                                        ..Default::default()
                                    },
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            } else {
                // 延后帧：从可见区顶部起逐帧往下补真颜色（其余行空样式占位），并预约
                // 下一帧继续推进；补齐（缓存覆盖整个请求窗）后不再预约，自动静止
                // （无空转帧）。
                let (lines, range, done) =
                    self.syntect_fill_deferred(lo_l, hi_l, vis_top_l);
                colored_range = range;
                if !done {
                    window.request_animation_frame();
                }
                lines
            }
        };
        // 输出按**视觉行**对齐（第 0 项 = 视觉 `lo` 行）：每个视觉行映射一个逻辑行。
        let version = self.buffer.version();
        // 光标所在行判定（selection head 落行内即高亮）：与 `row_caret` 的头部窗口
        // 判定同源口径，但取 head 一次性快照、不受光标闪烁影响（稳定持焦高亮）。
        // **选区坍缩守卫**：存在非坍缩选区（文字被选中）时不高亮整行——只高亮选中
        // 的字符本身（`is_collapsed` 与 `row_highlight_override` 同款判法），避免
        // 鼠标选中某行字符时整行再叠一层底色喧宾夺主。纯点击/光标（选区坍缩）时才
        // 显示光标行整行高亮。
        let head = self.selection.head();
        let head_selection_collapsed = self.selection.is_collapsed();
        let mut out: Vec<RowData> = Vec::with_capacity(hi - lo + 1);
        for v in lo..=hi {
            let logical = self.folds.logical_row(v);
            // 防御：`logical - lo_l` 在折叠映射异常时可能下溢（logical < lo_l → usize
            // 下溢）或上溢（logical > hi_l → idx ≥ hl_lines.len() 越界索引 panic）；
            // 对齐 `fill_place` 的双侧守卫（checked_sub + idx < len）——正常单调映射下
            // 恒等于 lo_l 行槽位索引（逻辑行 ≡ lo_l 时取 0），映射不一致时以空样式兜底
            // （不崩、此行不高亮），不改行内容。
            let base_styles: &[(Range<usize>, HighlightStyle)] = logical
                .checked_sub(lo_l)
                .and_then(|i| hl_lines.get(i))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            // 括号字符前景注入：本行有括号字符 → 对 `col..col+1` span 覆
            // `bracket_guide_palette[depth % 3]`（与引导线同色池）；无 → 原样 Borrowed
            // 零分配。覆色后的 styles 即行缓存 `hl_styles`（probe 逐位比对自洽：缓存键
            // 只含 version+行号，括号表随后台扫描落地变化 → 与新鲜覆色比对 miss → 重物化，
            // 正确性由比对兜底，不额外失效）。
            let styles: Cow<[(Range<usize>, HighlightStyle)]> = {
                let chars = self.bracket_chars_on_line(logical);
                if chars.is_empty() {
                    Cow::Borrowed(base_styles)
                } else {
                    Cow::Owned(render::merge_color_overrides(
                        base_styles,
                        &self.bracket_char_depths[chars],
                        &self.bracket_guide_palette,
                    ))
                }
            };
            let active = focused
                && head_selection_collapsed
                && head >= self.buffer.line_start(logical)
                && head <= self.buffer.line_end(logical);
            // 静态内容层（M5-B2-3 行级缓存）：命中缓存（键 + 本帧新鲜 runs 逐位比对）
            // → `Rc` 浅复用；未命中 → 物化新进窗口的行并入缓存。分片补色帧里只有
            // 「本帧真颜色区间」内的行走缓存 probe/write；占位空色行**不入缓存**，
            // 直接物化（否则把空色冻结进缓存 → 颜色永久缺失）。内容只取决于
            // `(buffer.version(), 逻辑行)`，折叠/选区/光标是下面动态层，不影响命中。
            let cacheable = colored_range.is_some_and(|(cf, ch)| logical >= cf && logical <= ch);
            let content = if cacheable {
                match self.row_cache.probe(version, logical, styles.as_ref()) {
                    Some(c) => c,
                    None => {
                        let c = Rc::new(RowContent::materialize(
                            &self.buffer,
                            logical,
                            styles.as_ref(),
                        ));
                        self.row_cache.put(version, logical, c.clone());
                        c
                    }
                }
            } else {
                Rc::new(RowContent::materialize(&self.buffer, logical, styles.as_ref()))
            };
            // 选区合并层 + 查找高亮层：选区动态合并（`row_highlight_override`）之上再叠
            // find 匹配底色（当前命中/其余匹配）。find 已关闭或无本行命中 → 不分配。
            let mut hl_override = self.row_highlight_override(logical, &content);
            if self.find.is_active() {
                let ov = self.find_row_overlays(logical);
                if !ov.is_empty() {
                    let base: &[(Range<usize>, HighlightStyle)] =
                        hl_override.as_deref().unwrap_or(&content.highlights);
                    hl_override = Some(render::merge_multi_highlights(base, &ov));
                }
            }
            out.push(RowData {
                highlight_override: hl_override,
                caret: self.row_caret(window, logical, show_caret),
                folded: self.row_folded(window, logical),
                active,
                content,
            });
        }
        crate::prof::end("ed.build_rows", _t_br);
        out
    }

    /// 延后帧「分片补色」：本帧只从可见区顶部 `vis_top_l` 起解析一截真颜色
    /// （`highlight_window` 命中/`extend_down` 增量续解析，不整窗重锚），窗口其余行
    /// 空样式。返回 `hl_lines`（对齐 `[lo_l, hi_l]`）+ 本帧真颜色的逻辑行区间
    /// （供 `build_rows` 行缓存写入口径：真色行走缓存、占位空色不入缓存）+ 是否已
    /// 整窗覆盖（`done=true` → 调用方不再预约下一帧，静止）。
    ///
    /// 相邻帧窗口一致且未编辑 → 游标继续下推（每次前缀变长 `HL_FILL_BUDGET_ROWS`）；
    /// 窗口变了（仍在拖）/编辑器版本变了（编辑）→ 游标重锚回新窗口可见顶。下方
    /// （`vis_top_l → hi_l`）补完后，剩余「上方过扫 `[lo_l, vis_top_l)`」由滚动停稳的
    /// `settle_pending` 一次有界重锚补齐（本分支兜底：若 settle 未触发，也在下方补完后
    /// 自己补一次整窗，保证缓存覆盖整个请求窗 → 下帧 `accept_deferred` 命中自动清
    /// pending）。整窗覆盖后返回 `done=true`（无空转重绘，项目红线）。
    fn syntect_fill_deferred(
        &mut self,
        lo_l: usize,
        hi_l: usize,
        vis_top_l: usize,
    ) -> (
        Vec<Vec<(Range<usize>, HighlightStyle)>>,
        Option<(usize, usize)>,
        bool,
    ) {
        let version = self.buffer.version();
        // 窗口变了（仍在拖）/ 编辑器版本变了（编辑）→ 重锚回新窗口可见顶。
        // （`cursor > hi_l` 不是重锚触发：那是「下方已补完」，下面沿用会话继续收尾。）
        if self.hl_fill.window != (lo_l, hi_l) || self.hl_fill.version != version {
            self.hl_fill = HlFill {
                window: (lo_l, hi_l),
                version,
                vis_top_l,
                cursor: vis_top_l,
                first_pass_done: false,
            };
        }
        let start = self.hl_fill.vis_top_l;
        let mut hl_lines: Vec<Vec<(Range<usize>, HighlightStyle)>> =
            Vec::with_capacity(hi_l - lo_l + 1);
        hl_lines.resize(hi_l - lo_l + 1, Vec::new());
        let mut colored: Option<(usize, usize)> = None;
        // P6：打开首帧零解析——仅在**文档顶部**（`vis_top==0`、高亮缓存仍为空，即首块刚
        // 挂载的开场帧）本帧**不解析任何 syntect**，只渲染纯文本，让「文件开头」在首个
        // 刷新帧即刻可见（省掉 ~50ms 的 32 行切片解析；颜色下一帧起每帧 32 行往下补）。
        // 只跳一次（`first_pass_done`）。滚动/深跳窗口（`vis_top>0`）照常首帧即补可见顶
        // 颜色（那是已展开会话，无「空白开场」问题）。
        if !self.hl_fill.first_pass_done
            && vis_top_l == 0
            && lo_l == 0
            && self.highlighter.is_cache_empty()
        {
            self.hl_fill.first_pass_done = true;
            return (hl_lines, None, false); // 未覆盖窗口 → 调用方预约下一帧开始补色
        }
        if self.hl_fill.cursor <= hi_l {
            // 本帧可达上限：游标向下推 `BUDGET` 行。
            let fetch_hi = self
                .hl_fill
                .cursor
                .saturating_add(HL_FILL_BUDGET_ROWS)
                .saturating_sub(1)
                .min(hi_l);
            // 请求整段前缀 `[start, fetch_hi]`：前半段在上帧已入缓存（命中、零解析），
            // 只新解析本帧多出的 `BUDGET` 行（`extend_down`，单帧成本可控）。
            let slice = self
                .highlighter
                .highlight_window(&self.buffer, start, fetch_hi, HL_LOOKBACK_LINES)
                .to_vec();
            for (i, runs) in slice.into_iter().enumerate() {
                let logical = start + i;
                let len = hl_lines.len();
                self.fill_place(&mut hl_lines, lo_l, len, logical, runs);
            }
            colored = Some((start, fetch_hi));
            self.hl_fill.cursor = fetch_hi + 1;
        }
        // 下方（可见区顶 → 窗口底）已补完：把整段 `[start, hi_l]` 取回渲染（命中缓存）。
        // 若缓存仍未盖到窗口顶（上方过扫 `[lo_l, start)` 未入缓存 / `base > lo_l`），先补
        // 一次整窗有界重锚（O(窗口+回看)≈一屏，`settle_pending` 同款）。正常路径滚动停稳
        // 时 settle 分支会先做这件事，此处是绕过 settle 的兜底。
        if self.hl_fill.cursor > hi_l {
            if self.highlighter.base() > lo_l {
                self.highlighter
                    .highlight_window(&self.buffer, lo_l, hi_l, HL_LOOKBACK_LINES);
            }
            self.hl_fill.cursor = hi_l + 1;
            let slice = self
                .highlighter
                .highlight_window(&self.buffer, start, hi_l, HL_LOOKBACK_LINES)
                .to_vec();
            for (i, runs) in slice.into_iter().enumerate() {
                let logical = start + i;
                let len = hl_lines.len();
                self.fill_place(&mut hl_lines, lo_l, len, logical, runs);
            }
            colored = Some((start, hi_l));
        }
        // 整窗覆盖 → `done`（调用方不再预约下一帧，静止）。
        let done = self.hl_fill.cursor > hi_l && self.highlighter.base() <= lo_l;
        (hl_lines, colored, done)
    }

    /// `syntect_fill_deferred` 的公共小段：把逻辑行 `logical` 的 runs 放进对齐 `[lo_l, ...)`
    /// 的 `hl_lines` 槽位（越界忽略）。
    fn fill_place(
        &self,
        hl_lines: &mut [Vec<(Range<usize>, HighlightStyle)>],
        lo_l: usize,
        len: usize,
        logical: usize,
        runs: Vec<(Range<usize>, Hsla)>,
    ) {
        if let Some(idx) = logical.checked_sub(lo_l)
            && idx < len
        {
            hl_lines[idx] = runs
                .into_iter()
                .map(|(r, c)| {
                    (
                        r,
                        HighlightStyle {
                            color: Some(c),
                            ..Default::default()
                        },
                    )
                })
                .collect();
        }
    }

    /// 测试：直接走一帧「分片补色」（不经过渲染帧，无 RAF），返回
    /// `(本帧真色起点, 本帧真色终点, 是否已整窗覆盖)`——供确定性回归锁定
    /// 「首帧补顶部、同窗口前缀变长、窗口变重锚、补满即停」的分片状态机。
    #[cfg(test)]
    pub(crate) fn test_fill_step(
        &mut self,
        lo_l: usize,
        hi_l: usize,
        vis_top_l: usize,
    ) -> ((usize, usize), bool) {
        let (_, range, done) = self.syntect_fill_deferred(lo_l, hi_l, vis_top_l);
        (range.unwrap_or((0, 0)), done)
    }
}
