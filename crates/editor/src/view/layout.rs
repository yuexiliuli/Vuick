//! 布局度量 / 滚动 / 命中 / 行几何。`CodeEditor` 的字体度量（`refresh_metrics`）、
//! ListState 固定窗口重定位（`resync_list`）、纵向/横向滚动与锚定揭示（`reveal_*`）、
//! 行几何查询（`row_layout` / `ruler_screen_x`）与点命中（`offset_at_point`）。

use gpui::{
    point,
    px,
    App,
    Bounds,
    Hsla,
    ListOffset,
    ListState,
    Pixels,
    Point,
    Window,
};

use super::{
    CodeEditor,
    FOLD_ARROW_SLOT_PX,
    GUTTER_LEFT_PAD_PX,
    GUTTER_RIGHT_PAD_PX,
    HSCROLL_MARGIN_COLS,
    LIST_WINDOW_ROWS,
};
use config::limits::{
    EDITOR_VSCROLL_BOTTOM_EXTRA_LINES,
};

/// 渲染行吸附行距（pitch）：`ceil(lh − 0.5)` 夹低到 ≥1.0——rhtz 正值变体
/// （`ceil(lh·sf−0.5)`，实证 = taffy 预吸附）。入参为**已按 `sf` 缩放**的行高：
/// 逻辑行距 = `pitch_for_line_height(lh * sf) / sf`；设备像素行距 =
/// `pitch_for_line_height(lh * sf)`（见 `snap_guide_extent`）。段几何四方共用，
/// 防同一吸附公式逐处复制再漂移。
pub(crate) fn pitch_for_line_height(lh: f32) -> f32 {
    ((lh - 0.5).ceil()).max(1.0)
}

impl CodeEditor {
    /// 列表视口高（px，测试断言用：List 必须撑满容器，否则可见行数为 0）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn list_viewport_height(&self) -> f32 {
        self.list_state.viewport_bounds().size.height.as_f32()
    }

    /// 竖向最大滚动量（px，测试断言用；像素路径含底部预留，滚到底时最后一行在
    /// 横向滚动条之上）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn list_max_offset(&self) -> f32 {
        (self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
            - self.list_state.viewport_bounds().size.height.as_f32())
        .max(0.)
    }

    /// 纵向量程总高（px）。像素路径 = 条目总数 × 行高（含底部预留空行；确定性），
    /// 诊断/测试用。
    pub fn vertical_total_px(&self) -> f32 {
        self.scroll_item_count() as f32 * self.line_height.as_f32().max(1.)
    }

    /// 测试：暴露 ListState（共享 Rc）——测试拖拽路径 / 量程对账。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_list_state(&self) -> ListState {
        self.list_state.clone()
    }

    /// 固定窗口虚拟化：列表始终是就绪的固定窗口（无两段式中间态），恒 false。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_list_is_deferred(&self) -> bool {
        false
    }

    /// 测试：ListState 固定窗口条数（= LIST_WINDOW_ROWS，与文档行数无关）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_list_window_len(&self) -> usize {
        self.list_state.item_count()
    }

    /// 测试：当前固定窗口首行绝对行号。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_window_base(&self) -> usize {
        self.window_base
    }

    /// 测试：文本区竖向滚动顶 `(绝对行号, offset_in_item)`。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_text_scroll_top(&self) -> (usize, f32) {
        self.abs_scroll_top()
    }

    /// 测试：行号列竖向滚动顶——行号是文本列表的叠层（同一绝对滚动顶），恒等于
    /// [`Self::test_text_scroll_top`]。
    #[cfg(test)]
    pub(crate) fn test_gutter_scroll_top(&self) -> (usize, f32) {
        self.abs_scroll_top()
    }

    /// 测试:用户请求的竖向滚动目标（px，绝对坐标）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_desired_scroll_px(&self) -> f32 {
        self.desired_scroll.get()
    }

    /// 测试：纵向底部预留（px，已量化到行高整数倍）。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_bottom_pad_px(&self) -> f32 {
        self.vscroll_bottom_pad_px()
    }

    /// 测试：列表条目总数（文档行数 + 底部预留空行）。增长完成后
    /// `test_list_state().item_count() == test_scroll_item_count()`。
    // cfg(test) 探针面：拆 crate 后被 markdown 测试跨 crate 消费，转无条件 pub。
    pub fn test_scroll_item_count(&self) -> usize {
        self.scroll_item_count()
    }

    /// 测试：直接滚到「量程最大值（含底部预留空行）」——复现滚到底的真实渲染结果。
    // cfg(test) 探针面：拆 editor crate 后被 app 测试跨 crate 消费，转无条件 pub。
    pub fn test_scroll_to_bottom(&mut self) {
        self.scroll_to_px(self.list_max_offset());
    }

    /// 测试：滚到目标内容 px（夹到量程内）——远跳窗口（大缺口滚动）用。
    #[cfg(test)]
    pub(crate) fn test_scroll_to_px(&mut self, target_px: f32) {
        self.scroll_to_px(target_px);
    }

    /// 设置元素包围盒（备用宽度来源）。内嵌宿主（数学编辑卡等浮层）在**首次渲染前**
    /// 用它给编辑器一个确定的可视宽——`sync_render_state` 的 `avail_w` 在
    /// `element_bounds` 未就绪（元素尚未 prepaint）时回退窗口宽，浮层内嵌编辑器首帧
    /// 会按窗口宽算 `content_w_px`/`visible_chars`/横向量程（长内容尾部不可达）。
    /// prepaint 每帧会用真实布局包围盒覆写本字段，故该值只影响首帧、随后自然收敛。
    pub fn set_element_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.element_bounds = bounds;
    }

    /// 设置横向内容的**尾部留白**（px；`0.` 关闭 = 默认）。仅当最宽行溢出可视区时，
    /// `compute_content_width` 会在内容宽上追加该值——滚到底后末尾字符与右裁切缘之间
    /// 留出空隙（避免尾部字符贴缘被裁/难以点中）。数学编辑卡长公式用；主编辑器不动。
    pub fn set_hscroll_tail_gap(&mut self, px: f32) {
        self.hscroll_tail_gap = px.max(0.);
    }

    // -----------------------------------------------------------------
    // 度量与布局
    // -----------------------------------------------------------------

    pub(super) fn refresh_metrics(&mut self, window: &mut Window, cx: &mut App) {
        // 第 3 波迁移：typography/语义色单一来自 `ui::theme`（不再读
        // gpui-component 主题 token）。等宽字体族与行高比例、缺省正文字号都由
        // `Theme::typography` 收敛（统一缩放架构，见 docs/ui-migration.md）。
        let theme = ui::theme::Theme::global(cx);
        let typo = &theme.typography;
        let colors = theme.colors;
        let dark = theme.is_dark();

        let mono = typo.mono.clone();
        // 字号覆盖：应用 Ctrl+=/- 调节（默认 None = 编辑器等宽缺省字号/行高，保留
        // gpui-component `mono_md` 的 13/20 既有默认；应用侧恒有 override=14）。
        let (font_size, line_height) = match self.font_override {
            Some(px_override) => (
                px(px_override),
                px(px_override * typo.editor_line_height_ratio),
            ),
            None => (
                px(typo.editor_default_size),
                px(typo.editor_default_size * typo.editor_line_height_ratio),
            ),
        };

        // 昂贵字体度量指纹门控：`resolve_font`/`ch_advance` 的唯一输入是上文算出的
        // `mono` 等宽字体族 + `font_size` 字号，二者未变则字符宽必不变——直接跳过
        // 昂贵路径（颜色等便宜字段仍在下方逐帧无条件写，不被门控）。gpui 全局对象
        // 没有变更通知（`Theme::global` 仅 TypeMap 查找，无 on_global 事件索引/版本），
        // 主题切换只能由「下一帧读到的主题内容」体现：字体相关变化 → 指纹变 → 重算；
        // 颜色变化 → 由无门控的字段写入兜底。首帧 `metrics_ready=false` / 指纹缺失
        // 必算；`set_font_override` 置 `metrics_ready=false` 的旧语义保留（双保险）。
        let metric_fp = (mono.clone(), font_size);
        if !self.metrics_ready || self.metric_fp.as_ref() != Some(&metric_fp) {
            let text_system = window.text_system();
            let font = gpui::font(mono.clone());
            self.char_width = text_system
                .ch_advance(text_system.resolve_font(&font), font_size)
                .unwrap_or(px(font_size.as_f32() * 0.6));
            self.metric_fp = Some(metric_fp);
            self.metrics_ready = true;
            self.metric_compute_count += 1;
            // 字体/字号变 → 每字符真实宽变 → 前缀宽索引全部作废（宽度与字形挂钩，
            // 与 `RowCache` 的内容缓存正交，独立失效；下帧按需重建）。
            self.row_width_cache.clear();
        }

        // 字号缩放（Ctrl+滚轮 / Ctrl+=/-）会同比改变行高：把「用户滚动目标」按行高比
        // 换算，把视口顶部的内容点锚定住（同 PDF 的视口锚定）——否则放大/缩小的瞬间
        // 行高变了但滚动 px 没变，正文会“跳”（窗口也随之滚动）。首次布局（行高 0）
        // 与行高不变时不动作。横向以 h_handle 的像素偏移为准，天然保持像素锚点。
        let old_lh = self.line_height.as_f32();
        let new_lh = line_height.as_f32();
        if old_lh > 0. && new_lh > 0. && (new_lh - old_lh).abs() >= 0.01 {
            let d = self.desired_scroll.get();
            self.desired_scroll.set(d * (new_lh / old_lh));
        }

        self.mono = mono;
        self.font_size = font_size;
        self.line_height = line_height;
        // 指标就绪：把「就绪前暂存的滚动目标逻辑行」按真实行高落地（修「md 重开编辑区
        // 被拉回顶部」：apply_text_restore 的 scroll_to_line 在首帧 paint 前触发，当时
        // line_height=0 只能存逻辑行；这里换算成真实 px 后同一帧 apply_desired_to_window
        // 即落窗口）。幂等：无暂存/已就绪时为 None，零开销。
        if let Some(line) = self.pending_scroll_line.take()
            && new_lh > 0.
        {
            let vis = self.folds.visual_row(line);
            self.scroll_to_px(vis as f32 * new_lh);
        }
        // 主题色/画布底色/行号列宽等便宜字段：每帧无条件写（主题切换后下一帧立即
        // 生效，成本可忽略，不与昂贵字体度量共用门控）。
        // 合并后 theme `chrome.editor_bg`（P1-2b：不读 `for_mode` 常量，预设/自定义
        // chrome.editor_bg 覆盖对编辑区底生效）。默认零覆盖与 `#121314` 逐字节一致。
        self.bg = ui::shell::content_bg_theme(cx, dark, colors.bg.into());
        self.fg = colors.fg.into();
        self.gutter_fg = colors.fg_muted.into();
        // 垂直标尺线：次要前景（与行号同源、已验证明暗主题都可见；不用 `border`——
        // 主题默认可能为透明）。细微参考线不刺眼。
        self.ruler_color = colors.fg_muted.opacity(0.55).into();
        // 缩进引导线竖线：VSCode 默认灰（非 active 档，走 ui::theme `guide_inactive`，
        // 2026-11-14「完全对齐 VSCode 默认主题」）。
        self.guide_color = colors.guide_inactive.into();
        // 点亮竖线（active 纯缩进组高亮）= VSCode 默认 active 灰 `guide_active`（用户口径
        // 「完全对齐 VSCode 默认主题」；曾经的强调蓝 accent × alpha 与顶部 L 形横线均已去除）。
        self.active_guide_color = colors.guide_active.into();
        // 括号引导线 3 色循环池（bracket guides，VSCode 口径）：`[Rgba; 3]` → `[Hsla; 3]`
        // 拷贝，bracket 叠加层按 `depth % 3` 取色（见 `bracket_guide_palette` 字段）。
        self.bracket_guide_palette = colors.bracket_guide_palette.map(|c| c.into());
        // 光标不能用 colors.accent：Light 主题 accent = #005fb8 蓝，白底上对比足够，
        // 但沿用既有口径用前景墨色，明/暗主题下都一定可见。
        self.caret_color = colors.fg.into();
        // 选区底色 = 主题 `editor_selection_bg`（精确不透明值，不再 accent×alpha 叠
        // 内容底派生——改调色板只动 theme，明/暗两主题各自可预测）。
        self.sel_bg = colors.editor_selection_bg.into();
        // 已折叠行的整行底色 = 主题 `editor_fold_bg`（比选区更淡、更贴内容底。
        // 锁「比选区弱」的回归见 tests/view.rs）。
        self.fold_bg = colors.editor_fold_bg.into();
        // 光标所在行（持焦且 head 落行内）的整行高亮底色 = 主题 `editor_active_line_bg`
        // （与折叠行同色；稳定持焦高亮，不受光标闪烁影响）。
        self.active_line_bg = colors.editor_active_line_bg.into();
        // 查找高亮两色（当前命中 / 其余匹配）。
        self.find_match_bg = colors.editor_find_match_bg.into();
        self.find_current_bg = colors.editor_find_current_bg.into();

        let digits = self.buffer.line_count().max(1).to_string().len();
        self.gutter_chars = digits.max(3);
        // 总宽 = 数字格 + 左右内边距 + 固定箭头槽（槽替代旧 +4 缓冲：数字格因此
        // 恒 = gutter_chars×字宽，行号右缘锚点固定，不会被 overflow_hidden 裁槽）。
        self.gutter_width_px = px(
            self.gutter_chars as f32 * self.char_width.as_f32()
                + GUTTER_LEFT_PAD_PX
                + GUTTER_RIGHT_PAD_PX
                + FOLD_ARROW_SLOT_PX,
        );
    }

    /// 重建 ListState 为固定窗口 `[0, LIST_WINDOW_ROWS)` 并回顶。首次 / 整篇替换 /
    /// 撤销重做都走这里：O(窗口) 常数，与文档规模无关——**不再是两段式 + 分帧增长**
    /// （改固定窗口虚拟化后打开大文件瞬时 O(1)，无需增长机制）。
    ///
    /// `defer_scans`：`true` = 撤销/重做等「文档行数已变、避免 UI 线程整篇重扫」——
    /// 折叠候选/括号配对改为**下一帧挂起后台刷新**（[`Self::scan_dirty_edit`]）；
    /// `false` = 整篇替换/打开（内容全新，同步重扫一次，首屏即就绪，测试依赖）。
    pub(super) fn resync_list(&mut self, defer_scans: bool) {
        let new_n = self.buffer.line_count().max(1);
        self.row_count = new_n;
        let t = std::time::Instant::now();
        self.list_state.reset(LIST_WINDOW_ROWS);
        self.window_base = 0;
        self.desired_scroll.set(0.);
        // 硬化：`resync_list` 回顶后暂存逻辑行坐标已失义（list 重挂）——清位防「字体
        // 覆盖 + 恢复期 undo」等未来路径把暂存值覆写到 reveal_caret 落点。评审已核：
        // md 重开/重驱动流中 resync 均发生在暂存之前，此处清位不破坏主修复，仅兜底。
        self.pending_scroll_line = None;
        // 诊断：`resync_list` 把滚动目标清零 + 列表回顶的调用点（真机「47→2」若此处命中
        // 即内容重挂导致的回顶）。
        crate::diag::ed_scroll(
            "resync_list.desired_zero",
            &format!("row_count={new_n} defer_scans={defer_scans}"),
        );
        // 整段重挂：滞留的高亮增量登记作废（撤销/重做后走整段重解析）。
        self.hl_dirty = None;
        // 折叠投影总行数跟随文档行数（区间越界由 FoldMap 侧裁剪）。
        self.folds.set_total(new_n);
        if defer_scans {
            // 撤销/重做路径：增量描述由 undo/redo 先行登记（`scan_pending_edit` /
            // `scan_multi_edit`），不在这里整篇重扫（大文档 ~280ms UI 卡顿），
            // 交给下一帧后台刷新做增量/整篇重算。
            self.scan_dirty_edit = true;
        } else {
            // 打开/整篇替换路径：同步重扫一次（内容全新，首屏即就绪）。
            self.recompute_fold_brackets_full();
        }
        if std::env::var_os("MD_OPEN_DEBUG").is_some() {
            eprintln!(
                "[MD_OPEN_DEBUG] 编辑器 ListState 固定窗口 reset({LIST_WINDOW_ROWS})，{new_n} 行 + {} 预留空行: {} ms",
                self.pad_row_count(),
                t.elapsed().as_millis()
            );
        }
    }

    /// 由可视宽推导「可见宽度单位格数」：真实可视格 + 8 的估算余量（多余量供折行/
    /// 绘制安全，`reveal_caret` 已先减掉再算右缘）。**不再设 80 格下限**——该下限是
    /// 「按窗口切片渲染文本」时代的遗留：窄视口（数学编辑卡 ~57 格）会被虚高到 80，
    /// `reveal_caret` 的右缘按 80 算 → 光标/新输入停在真实右缘之外、被 `overflow_hidden`
    /// 裁掉看不见。主编辑器视口 ≫80 格，该下限从不生效、行为不变。
    pub(super) fn visible_chars_from_width(&self, width_px: f32) -> usize {
        let cw = self.char_width.as_f32().max(1.);
        (((width_px - self.gutter_width_px.as_f32()).max(0.) / cw).ceil() as usize)
            .saturating_add(8)
            .max(1)
    }

    // -----------------------------------------------------------------
    // 纵向滚动：像素级确定性换算（固定窗口虚拟化的像素路径，恒启用）
    // -----------------------------------------------------------------
    // 像素模式下条目高度**不预铺 uniform**（None 提示按需实测），ListState 的
    // summary 高度全是 0，所以一切滚动都不能再走 summary 换算（scroll_by /
    // scroll_to_reveal_item / max_offset 相关），统一由这里以「行数×行高」的
    // 确定性像素模型换算成 ListOffset，再交给 `list_state.scroll_to`（只看 item_count，
    // 不看高度）。量程则全由 VerticalScrollHandle 的 content_height 承担。

    /// 纵向最大滚动量的**底部预留**（px）= 横向滚动条厚度 + 额外行数×行高，
    /// **向上量化到 line_height 的整数倍**：叠加在底部的横向滚动条厚度（16px）
    /// 通常不是行高的整数倍，若不量化，滚到底时视口停在「行间半格」上，最后一行
    /// 加载/点击命中/光标位置出现次像素误差。量化后一切纵向偏移都是整行像素，
    /// 宁可多留白半行也绝不出现 0.5px 的渲染模糊。
    fn vscroll_bottom_pad_px(&self) -> f32 {
        let lh = self.line_height.as_f32().max(1.);
        let raw = ui::scrollbar::scrollbar_width().as_f32()
            + EDITOR_VSCROLL_BOTTOM_EXTRA_LINES as f32 * lh;
        (raw / lh).ceil() * lh
    }

    /// 底部预留换算成的行数（`vscroll_bottom_pad_px` 已量化到行高整数倍，故整除）。
    ///
    /// 固定窗口虚拟化下预留区**不是**列表里的真实条目，而是「滚动上限包含它」：
    /// 滚到底时视口顶行 = `scroll_item_count − 可见行数`，该行落在文档正文内，窗口
    /// 从它往下渲染自然盖住预留区（超出 `row_count` 的窗口条目渲染为空行 = 预留空白，
    /// 见 render 的 RowView 映射）。gpui「自动填满视口」只会把窗口内的**空行**拉到
    /// 视口底，预留区仍是可见的空白。
    fn pad_row_count(&self) -> usize {
        let lh = self.line_height.as_f32().max(1.);
        (self.vscroll_bottom_pad_px() / lh).round() as usize
    }

    /// 滚动涉及的**全文条目数** = 视觉行数（逻辑行 − 折叠隐藏行）+ 底部预留空行数。
    /// 纵向滚动上限 / 量程 / clamp 全部以它为准（与固定窗口无关——窗口只是
    /// 「从哪里开始渲染」）。折叠会缩小它（与窗口大小无关，thumb 无坍缩）。
    pub(super) fn scroll_item_count(&self) -> usize {
        self.folds
            .visual_line_count()
            .max(1)
            .saturating_add(self.pad_row_count())
    }

    /// 文本区 **绝对**滚动位置 `(绝对行号, 已滚进该行的 px)`。窗口内的相对
    /// `item_ix` 由 [`Self::window_base`] 抬成绝对行；行号/命中/渲染全文都以它为准。
    pub(super) fn abs_scroll_top(&self) -> (usize, f32) {
        let s = self.list_state.logical_scroll_top();
        (self.window_base + s.item_ix, s.offset_in_item.as_f32())
    }

    /// 视口顶部的**源码行**（0 基；绝对视觉行经折叠映射回逻辑行）。编辑区⇄预览
    /// 滚动同步用——同步单位 = 源码行，两侧都以它对齐。
    pub fn viewport_top_line(&self) -> usize {
        let (vis, _) = self.abs_scroll_top();
        self.folds.logical_row(vis)
    }

    /// 滚动到目标**源码行**置于视口顶部（统一行高模型；逻辑行经折叠映射成视觉行，
    /// 再换算像素交给 `scroll_to_px`——夹到量程内，底部预留让末行停在横向条上方）。
    /// `#[track_caller]`：配合 `scroll_to_px` 的同类标注，`ed_scroll` 探针能打出
    /// **最外层**真实调用点（apply_text_restore / sync 各分支 / find_goto …）。
    ///
    /// 指标未就绪（首帧布局/重挂前 `line_height`=0，`metrics_ready=false`）时目标是
    /// **按行高换算 px** 的，直接用回退行高 `1.0` 会写进错误 px——随后首个真实指标落地
    /// 时 `apply_desired_to_window` 会把 `N×1px` 重映射到 `N/真实行高` 行（真机「md 重开：
    /// 编辑区被拉回顶端 47→2」根因：apply_text_restore 经 on_next_frame 先于新编辑区元素
    /// 首帧 paint 触发）。此时**暂存逻辑行** `pending_scroll_line`，指标就绪后由
    /// `refresh_metrics` 按真实行高落地。
    #[track_caller]
    pub fn scroll_to_line(&mut self, line: usize) {
        if !self.metrics_ready || self.line_height.as_f32() <= 0. {
            // 指标未就绪：只记目标逻辑行，不写 desired_scroll（防 px 错拍）。
            self.pending_scroll_line = Some(line);
            return;
        }
        self.pending_scroll_line = None;
        let lh = self.line_height.as_f32().max(1.);
        let vis = self.folds.visual_row(line);
        self.scroll_to_px(vis as f32 * lh);
    }

    /// ListState **实际**滚动位置（px，绝对坐标；读窗口合成，不读 `desired_scroll`）。
    /// 用于重建后把「用户目标」重新对齐 ListState（见 `resync_list`），以及常规读数。
    fn list_scroll_top_px(&self) -> f32 {
        let (abs_ix, off) = self.abs_scroll_top();
        let lh = self.line_height.as_f32().max(1.);
        abs_ix as f32 * lh + off
    }

    /// 当前竖向量程（px）= 全文条目数 × 行高 − 视口，夹 ≥0（绝对值，与窗口无关）。
    /// 滚动 clamp / 滚动条 thumb / 量程诊断共用这一份。
    pub(super) fn reach_px(&self) -> f32 {
        let lh = self.line_height.as_f32().max(1.);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        (self.scroll_item_count() as f32 * lh - view).max(0.)
    }

    /// 当前竖向滚动量（px，绝对坐标；0 = 顶部）。
    pub(super) fn scroll_top_px(&self) -> f32 {
        self.list_scroll_top_px()
    }

    /// 像素 → ListOffset（统一行高模型，窗口内相对坐标由调用方套 `window_base`）。
    /// 全 crate 无调用点（滚动/命中均直接走 `apply_desired_to_window` 的取整逻辑），
    /// dormancy 休眠保留（K24）。
    #[allow(dead_code)]
    fn px_to_offset(px: f32, lh: f32) -> ListOffset {
        let item_ix = (px / lh).floor().max(0.) as usize;
        let offset = px - item_ix as f32 * lh;
        ListOffset {
            item_ix,
            offset_in_item: gpui::px(offset.max(0.)),
        }
    }

    /// 把固定窗口对齐到用户目标（`desired_scroll`，绝对 px）：目标窗口覆盖不足一屏则
    /// 重定位 `window_base = abs_top` 并 `reset(LIST_WINDOW_ROWS)`（O(窗口)，任意跳转
    /// 瞬时可达，无分帧、无占位符）；窗口内则直接 `scroll_to` 相对位置。滚动条拖动只写
    /// `desired_scroll`，render 每帧调用本方法落窗口；滚轮/键盘/跳转也经它。
    pub(super) fn apply_desired_to_window(&mut self) {
        let lh = self.line_height.as_f32().max(1.);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let vis_rows = ((view / lh).ceil() as usize).max(1);
        let target = self.desired_scroll.get().clamp(0., self.reach_px());
        let abs_top = (target / lh).floor().max(0.) as usize;
        let frac = target - abs_top as f32 * lh;
        let rel = abs_top.saturating_sub(self.window_base);
        let covers = abs_top >= self.window_base && rel + vis_rows <= LIST_WINDOW_ROWS;
        if !covers {
            self.window_base = abs_top;
            self.list_state.reset(LIST_WINDOW_ROWS);
        }
        let rel = abs_top.saturating_sub(self.window_base);
        self.list_state.scroll_to(ListOffset {
            item_ix: rel,
            offset_in_item: px(frac),
        });
        self.desired_scroll.set(target);
    }

    /// 滚到目标像素（夹到 [0, 全文高−视口]），写入 `desired_scroll` 并落窗口。
    /// 底部预留让最后一行能滚到横向滚动条之上（见 `pad_row_count`）。
    /// `#[track_caller]`：诊断用——`crate::diag::ed_scroll` 据此打出**真实调用点**
    /// （editor 内部所有滚动都经此咽喉），真机「编辑区 47→2」靠此行定位来源。
    #[track_caller]
    pub(super) fn scroll_to_px(&mut self, target_px: f32) {
        let px = target_px.clamp(0., self.reach_px());
        let prev = self.desired_scroll.get();
        self.desired_scroll.set(px);
        // 探针默认关：只在开启时做 format（`ed_scroll` 内部再判一次 enabled，OnceLock
        // 近零成本）——热路径零分配、零格式化，遵守「探针不入热路径」。
        if crate::diag::enabled() {
            crate::diag::ed_scroll(
                &format!("{}", std::panic::Location::caller()),
                &format!(
                    "to_px={px:.1} prev={prev:.1} lh={:.1} top_vis={} top_logical={}",
                    self.line_height.as_f32().max(1.),
                    self.abs_scroll_top().0,
                    self.folds.logical_row(self.abs_scroll_top().0),
                ),
            );
        }
        self.apply_desired_to_window();
    }

    /// 滚轮竖向滚动（dy = 滚轮 delta，负=向下）。
    #[track_caller]
    pub(super) fn scroll_vertical(&mut self, dy: f32) {
        // WY_TRACE：滚轮竖向滚动登记一次 scroll 操作；编辑已在 insert_edit 起会话，
        // 不重复起（避免滚轮同帧把编辑会话误标成 scroll）。
        if !crate::prof::op_pending() {
            crate::prof::op_start("ed_scroll");
        }
        self.scroll_to_px(self.scroll_top_px() - dy);
    }

    /// 把目标**逻辑行**滚进可视区（统一行高模型；先经折叠映射成视觉行，再算像素：
    /// 折叠隐藏行的逻辑行会塌缩到折叠首行的视觉位置）。可视高度只扣除底部**横向
    /// 滚动条**（真正的非文本区），保证被 reveal 的行不会贴到横向滚动条之下。
    ///
    /// ⚠️ 这里**不能**把整段底部预留（`vscroll_bottom_pad_px` = 滚动条 16px + 8 行
    /// 呼吸区）当「无需滚动区」从可视高里扣——那是**量程**语义（滚到底让末行停在
    /// 滚动条之上，由 `reach_px`/`pad_row_count` 承担）。矮视口（数学编辑卡内嵌
    /// 编辑器高 200px、预留 ≈180px，见 P10 实测）全量扣预留 → `rows_visible`=1 →
    /// 点击任意非首行都触发 `scroll_to_px` 把该行顶到顶部（用户实测「点击某行直接
    /// 跳成第一行、内容滚下去」）。按真实可见行数算，点击任何可视行都不滚动。
    pub(super) fn reveal_row(&mut self, row: usize) {
        let lh = self.line_height.as_f32().max(1.);
        let vis = self.folds.visual_row(row);
        let view = self.list_state.viewport_bounds().size.height.as_f32();
        let scrollbar = ui::scrollbar::scrollbar_width().as_f32();
        // find 卡片（Ctrl+F）已改为编辑区**右上角**小卡片（见 `render_find_card`），
        // 不再像旧顶部通栏那样整段盖住可视上缘 → `FIND_BAR_H`（32px 整行偏移）已删，
        // 此处无需把「可视上缘」下移一条。顶部右上角卡片若正好压住当前命中属可接受
        // 现象（VS Code 同款），用户随手滚动即露。
        let rows_visible = ((view - scrollbar) / lh).floor().max(1.) as usize;
        let cur_top = (self.scroll_top_px() / lh).floor() as usize;
        if vis < cur_top {
            self.scroll_to_px(vis as f32 * lh);
        } else if vis >= cur_top + rows_visible {
            let target = vis.saturating_add(1).saturating_sub(rows_visible);
            self.scroll_to_px(target as f32 * lh);
        }
    }

    pub(super) fn reveal_caret(&mut self) {
        // WY_TRACE breakdown：编辑/移动后的「把光标滚进可视区」段（纵向 + 横向）。
        let _t_reveal = crate::prof::begin();
        let head = self.selection.head();
        let row = self.buffer.line_of_offset(head);
        self.reveal_row(row);

        // 横向 reveal 用「宽度单位」列（CJK=2 格）与可视格数一致，否则中文长行
        // 少滚一半、光标仍跑出可视区（用户实测：内容超窗后新输入看不见）。
        // 注意 `visible_chars` 带有 +8 的估算余量（visible_chars_from_width），
        // 若直接当可视格数用，`right_edge` 会偏大 8 格 → 光标停在真实右缘之外 4 格
        // （用户实测：长行末尾输入后最新内容看不到）。先减掉余量得真实可视格。
        let col = self.buffer.unit_column(head);
        let margin = HSCROLL_MARGIN_COLS;
        let view_cells = self.visible_chars.saturating_sub(8).max(1);
        let right_edge = view_cells.saturating_sub(margin).max(1);
        if col < self.hscroll_cols + margin
            || col >= self.hscroll_cols.saturating_add(right_edge)
        {
            // 左缘触发（col 在视口左缘 margin 内，含已滚到最左、col≥margin 的情形）：
            // 目标 = col - margin，让光标保留 margin 格左边上下文；col<margin 时为
            // 0，不产生无意义的向右滚动——否则空行/行首打字会把内容逐格左移、到
            // margin 边界又走右缘分支 snap 回 0，形成「界面往右弹又弹回」。
            // 右缘触发（col ≥ hscroll+right_edge）：目标 = col - right_edge。
            let target = if col < self.hscroll_cols + margin {
                col.saturating_sub(margin)
            } else {
                col.saturating_sub(right_edge)
            };
            self.set_hscroll(target);
        }
        crate::prof::end("ed.reveal", _t_reveal);
    }

    /// 仅竖向 reveal（鼠标点击/拖动用）。点击必落在可视区内，无需横向自动滚动；
    /// 若不区分，点行首/空白行会把内容强行拉去最左 →「内容左右晃动」。
    pub(super) fn reveal_caret_vertical(&mut self) {
        let row = self.buffer.line_of_offset(self.selection.head());
        self.reveal_row(row);
    }

    pub(super) fn set_hscroll(&mut self, cols: usize) {
        self.hscroll_cols = cols;
        let x = cols as f32 * self.char_width.as_f32();
        self.h_handle.set_offset(point(px(-x), px(0.)));
    }

    /// 按像素横向滚动（容器滚到底/顶自动夹紧），并同步 `hscroll_cols`。
    /// 量程 = 容器**实测** `h_handle.max_offset().x`（= `content_w_px` − 文本区可视宽，
    /// 与横向滚动条读取同一真源；上一帧布局值，下一帧 read-back 对齐，同竖向量程
    /// `reach_px` 的确定性口径）。亚格增量（<半格）直接丢弃：部分鼠标滚轮带轻微
    /// 水平分量/倾斜轮噪声，若每帧量化成整格会令内容（行号右缘）左右跳一格、
    /// 看起来像在“晃”。
    pub(super) fn scroll_horizontal_px(&mut self, dx_px: f32) {
        let cw = self.char_width.as_f32().max(1.);
        if dx_px.abs() < cw * 0.5 {
            return;
        }
        // WY_TRACE：横向滚动同竖向；有在途会话（编辑/竖向滚轮同帧）不重复起。
        if !crate::prof::op_pending() {
            crate::prof::op_start("ed_scroll");
        }
        let max_px = self.h_handle.max_offset().x.as_f32().max(0.);
        let cur_px = (-self.h_handle.offset().x.as_f32()).clamp(0.0, max_px);
        let target_px = (cur_px + dx_px).clamp(0.0, max_px);
        let cols = (target_px / cw).round() as usize;
        self.set_hscroll(cols);
    }

    /// 滚轮步长换算：`(行高, 字符宽)`。
    pub(super) fn scroll_factors(&self) -> (f32, f32) {
        (
            self.line_height.as_f32().max(1.),
            self.char_width.as_f32().max(1.),
        )
    }

    // -----------------------------------------------------------------
    // 坐标换算（元素本地坐标）
    // -----------------------------------------------------------------

    /// 当前横向滚动像素（内容被推向左的量；`h_handle` offset 为负）。
    pub(super) fn hscroll_px(&self) -> f32 {
        (-self.h_handle.offset().x.as_f32()).max(0.)
    }

    /// 列（宽度单位）→ **逻辑 px**（`col × char_width − hscroll_px`，editor 本地 x；不带
    /// 行号列偏移——调用方传给文本区本地叠层）。取整口径分层——竖线/缩进线上层自做
    /// `.round()`、钩线上层再叠设备像素 snap（render.rs:412）——本函数只出逻辑 px 不改
    /// 取整；竖线/缩进线/钩线三处 col→px 共用，防换算式再漂移（数值逐位不变）。
    pub(super) fn col_to_px(&self, col: usize) -> f32 {
        col as f32 * self.char_width.as_f32().max(1.) - self.hscroll_px()
    }

    /// 垂直标尺第 `col`（宽度单位列）列在**editor 本地 x**（px）。标尺随内容横向
    /// 滚动（方案 A）；移出文本区右缘或左滚出文本区（< 行号列右缘）→ `None`（被
    /// 裁掉/不可见）。`avail_w` = 编辑区可视宽（文本区右界）。
    pub(super) fn ruler_screen_x(&self, col: usize, avail_w: f32) -> Option<f32> {
        if !self.metrics_ready || self.char_width.as_f32() <= 0. {
            return None;
        }
        let gutter = self.gutter_width_px.as_f32();
        let view_w = (avail_w - gutter).max(0.);
        if view_w <= 0. {
            return None;
        }
        // col→px 复用 `col_to_px`（`col·cw − hscroll`，取整口径同源），gutter 偏移单独加。
        let x = gutter + self.col_to_px(col);
        if x >= gutter && x < gutter + view_w {
            Some(x.round().max(gutter))
        } else {
            None
        }
    }

    /// 整行文本整形为布局（与 `StyledText` 渲染同一个 font/size）。
    ///
    /// 供命中测试 / 光标 x / IME 坐标使用：CJK 全角字按真实字形宽度定位，
    /// 不再用 `col × char_width` 等宽近似（否则中文行点击/光标整体后移）。
    /// `TextSystem` 内置行布局缓存，同一行同帧重复整形是缓存命中，开销≈0。
    pub(super) fn row_layout(&self, window: &mut Window, row: usize) -> Option<gpui::ShapedLine> {
        if !self.metrics_ready || self.font_size.as_f32() <= 0. {
            return None;
        }
        let text = self.buffer.line_text(row);
        if text.is_empty() {
            return None;
        }
        let run = gpui::TextRun {
            len: text.len(),
            font: gpui::font(self.mono.clone()),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        Some(
            window
                .text_system()
                .shape_line(text, self.font_size, &[run], None),
        )
    }

    pub(super) fn offset_at_point(&self, position: Point<Pixels>, window: &mut Window) -> usize {
        if self.row_count == 0 || self.line_height.as_f32() <= 0. {
            return 0;
        }
        let lh = self.line_height.as_f32();
        // 用「有效滚动顶」：深跳占位期 = 用户目标（点击占位符命中其代表的目标行），
        // 常规期 = ListState 实际位置（一致）。
        let content_y = self.scroll_top_px() + position.y.as_f32();
        // 命中换算到**视觉行**，再经折叠映射成逻辑行（视觉行可被折叠压缩）。
        let vis = (content_y / lh).floor().max(0.) as usize;
        let vis = vis.min(self.folds.visual_line_count().saturating_sub(1));
        let row = self.folds.logical_row(vis);
        let row = row.min(self.row_count.saturating_sub(1));

        // 命中的文本本地 x（补偿横向滚动像素）：x_text = local.x − gutter + 已滚像素。
        let line_start = self.buffer.line_start(row);
        let x_text = position.x.as_f32() - self.gutter_width_px.as_f32() + self.hscroll_px();
        if x_text <= 0. {
            return line_start;
        }
        if let Some(layout) = self.row_layout(window, row) {
            // closest_index_for_x 返回**行内字节索引**，且总落在字符边界上
            // （绝不把光标拆进 CJK/emoji 中间）。
            return self
                .buffer
                .clip(line_start + layout.closest_index_for_x(px(x_text)));
        }
        // 兜底（度量未就绪等）：等宽近似。
        let col_rel = (x_text / self.char_width.as_f32().max(1.)).floor() as usize;
        let line_chars = self.buffer.line_text(row).chars().count();
        let col = col_rel.min(line_chars);
        self.buffer.offset_for_char_column(row, col)
    }
}
