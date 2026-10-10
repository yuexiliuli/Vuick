use super::*;

use gpui::Pixels;
use ropey::Rope;

use crate::markdown::view::style::StyleParams;

/// WyEditor 核心生命周期 / 文本 API / 选区 / 剪贴板 / IME 直输（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ts = window.text_style();
        let fs: Pixels = ts.font_size.to_pixels(window.rem_size());
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        Self {
            doc: Doc::open(SharedString::default()),
            focus: cx.focus_handle(),
            caret: 0,
            loaded: false,
            editable: true,
            theme: WyTheme::from_style(&p, ts.font(), fs),
            frame: RefCell::new(None),
            last_bounds: Cell::new(None),
            zoom_anchor: RefCell::new(None),
            viewport_top_px: Cell::new(0.0),
            scroll_target: Cell::new(None),
            last_pin: Cell::new(None),
            scroll_applies: Cell::new(0),
            zoom_last_target: Cell::new(None),
            zoom_applies: Cell::new(0),
            host_scroll: RefCell::new(None),
            content_gen: Cell::new(0),
            shape_band: Cell::new(None),
            prewarm: RefCell::new(Prewarm::default()),
            block_layout: RefCell::new(None),
            struct_ver: Cell::new(0),
            struct_dirty_line: Cell::new(0),
            struct_old_end_line: Cell::new(0),
            struct_line_delta: Cell::new(0),
            struct_blk_old: Cell::new(0),
            struct_blk_span_delta: Cell::new(0),
            struct_edit_pending: Cell::new(false),
            regions_ver: Cell::new(0),
            fold_ver: Cell::new(0),
            fold_plan: RefCell::new(None),
            pending_patch_blocks: RefCell::new(None),
            math_dollar_cnt: Cell::new(0),
            alert_marker_cnt: Cell::new(0),
            derived: RefCell::new(None),
            shape_rows: RefCell::new(RowShapeCache::new(
                crate::config::limits::SHAPED_ROW_CACHE_CAP,
            )),
            // 含 `.notdef`（元字形，glyph id=0）的坏排板块：Windows 冷启动时回退字体
            // （❗⚠✅ 等主字体缺的字形）首次解析可能给出 .notdef，且坏的 ShapedRow 若被
            // 行缓存钉住整次运行不消失。此表登记「待新鲜重排」的块并驱动机器（见
            // [`NotdefRetry`]）：Pass2 命中判定/入缓存前都扫 .notdef，命中坏缓存视作
            // miss、出新 notdef 不入缓存，走「冷却一帧→重排」直至 DWrite 回退解析成功。
            notdef_retry: RefCell::new(NotdefRetry::default()),
            #[cfg(test)]
            reshapes: Cell::new(0),
            #[cfg(test)]
            prewarm_shapes: Cell::new(0),
            #[cfg(test)]
            coolings: Cell::new(0),
            image_cache: gpui::RetainAllImageCache::new(cx),
            image_sizes: RefCell::new(std::collections::HashMap::new()),
            images_gen: Cell::new(0),
            image_widths: RefCell::new(std::collections::HashMap::new()),
            image_src_blocks: RefCell::new(None),
            pending_image_srcs: RefCell::new(std::collections::HashSet::new()),
            image_resize_drag: RefCell::new(None),
            math_slots: Rc::new(RefCell::new(std::collections::HashMap::new())),
            math_color: RefCell::new(None),
            math_gen: Cell::new(0),
            mermaid_zoom: RefCell::new(std::collections::HashMap::new()),
            mermaid_zoom_gen: Cell::new(0),
            focused_mermaid: None,
            code_parse: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            code_parse_need: RefCell::new(Vec::new()),
            // 数学**显示行数**缓存（键=去定界符源）：`math_card_height` 回退分支按行数
            // 估高——`math_display_lines` 会对每个源做 LaTeX→Unicode 转换，3000 个数学
            // 块每次 fold_plan 重建重复转换 = ~38ms（B3）；缓存后 O(1) 复用。内容变更
            // 清缓存。
            math_line_map: RefCell::new(std::collections::HashMap::new()),
            inline_math_rows: RefCell::new(std::collections::HashMap::new()),
            math_pins: RefCell::new(std::collections::HashMap::new()),
            base_dir: None,
            marked: None,
            sel_anchor: None,
            dragging: false,
            toc_cache: RefCell::new(None),
            highlight_search: None,
            highlight_seq: 0,
            code_h_scroll: RefCell::new(std::collections::HashMap::new()),
            code_h_drag: RefCell::new(None),
            table_h_scroll: RefCell::new(std::collections::HashMap::new()),
            table_h_drag: RefCell::new(None),
            math_h_scroll: RefCell::new(std::collections::HashMap::new()),
            math_h_drag: RefCell::new(None),
            math_natural_w: RefCell::new(std::collections::HashMap::new()),
            folded_headings: RefCell::new(std::collections::HashSet::new()),
            block_regions: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_unit: None,
            last_edit: None,
            hover_image: Cell::new(None),
            selected_image: Cell::new(None),
            image_edit: None,
            image_edit_anchor: Cell::new(None),
        }
    }

    /// 覆盖主题。
    #[allow(dead_code)]
    pub fn set_theme(&mut self, theme: WyTheme) {
        self.theme = theme;
        *self.frame.borrow_mut() = None;
        // 基础布局存的是 px 真值，主题/字号变化量纲改变 → 代置失效位（否则带外沿用
        // 旧字号的固定 px 高，行高与带内新字号不一致，滚动跳变）。
        *self.block_layout.borrow_mut() = None;
        // 整形行内容同样依赖主题（字号/颜色/字体）→ 整体清空。
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
    }

    /// 显式设置字号（应用把全局字号同步进来；变更新帧缓存）。不读窗口 text_style，
    /// 保证 measure/prepaint 两阶段一致。字号不只影响正文/代码尺寸，标题也按
    /// `StyleParams::heading_metrics` 等比缩放（见 `WyTheme::set_font_size`）。
    pub fn set_font_size(&mut self, size: Pixels) {
        if self.theme.font_size == size {
            return;
        }
        // 缩放锚点：重排前记录视口顶部内容点，帧按新字号重建后由 element prepaint
        // 自驱动滚回（`advance_zoom_restore`）——Ctrl+滚轮缩放不带动滚动位置，
        // 对齐 view/editor。
        self.capture_zoom_anchor();
        self.theme.set_font_size(size);
        *self.frame.borrow_mut() = None;
        // 同 `set_theme`：字号变 → 基础布局（px）失效。
        *self.block_layout.borrow_mut() = None;
        // 字号变 → 行整形（宽/高）也失效。
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
    }

    /// 从全局主题**重建整份调色主题**（应用「切换黑白主题」后调用）：WyTheme 在
    /// `new()` 时由 [`StyleParams::from_theme_with_font`] 一次性派生并缓存（主题切换
    /// 后如不重建，预览/WYSIWYG 的文字/代码块/链接等颜色会保持旧主题色）。正文字体与
    /// 字号沿用本主题既有的（创建时从 `window.text_style()` 确定性取用），只重建色板。
    /// `set_theme` 会清空帧/布局/整形缓存，下一帧按新色板重画。
    pub(crate) fn rebuild_theme_from_global(&mut self, cx: &mut gpui::App) {
        let fs = self.theme.font_size;
        let body_font = self.theme.font.clone();
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        self.set_theme(WyTheme::from_style(&p, body_font, fs));
    }

    /// 切只读/可编辑门控（A1：MD 右栏预览用）。只读 = 输入/IME/删除/撤销/粘贴短路
    /// （`replace_range` 硬门控）、不画光标；保留光标移动、点选/复制、链接点击、折叠、
    /// 搜索高亮、表格/代码横向滚动条。切只读时丢弃 IME 组合态、收拢选区为光标（避免
    /// 只读态下残留活动选区误导）。已处目标态则 no-op（不触发帧失效）。
    pub fn set_editable(&mut self, editable: bool) {
        if self.editable == editable {
            return;
        }
        self.editable = editable;
        if !editable {
            self.marked = None;
            self.sel_anchor = None;
            self.dragging = false;
            // 只读：无编辑能力 → 收起图片编辑卡片与悬浮按钮态、图片选中态。
            self.image_edit = None;
            self.image_edit_anchor.set(None);
            self.hover_image.set(None);
            self.selected_image.set(None);
        }
        *self.frame.borrow_mut() = None;
    }

    /// 当前是否可编辑（只读预览 = `false`）。
    pub fn is_editable(&self) -> bool {
        self.editable
    }
    /// A4：目录条目 `(源码行, 标题级别, 标题文本)`——与 `MarkdownPreview::toc_entries`
    /// 同构，首字段语义 = **源码行**（统一组件宿主用 `scroll_to_line` 跳转）。遍历 Doc
    /// 块（O(块数)，按 `content_gen` 缓存，仅内容变更重扫；侧边栏目录渲染低频消费）。
    pub(crate) fn toc_entries(&self) -> TocEntries {
        if let Some((g, t)) = &*self.toc_cache.borrow()
            && *g == self.content_gen.get()
        {
            return t.clone();
        }
        let n = self.doc.block_count();
        let mut out = Vec::with_capacity(n.min(1024));
        for i in 0..n {
            let Some(m) = self.doc.block_meta(i) else {
                continue;
            };
            if let crate::markdown::parse::RowKind::Heading(level) = m.kind {
                let t = self.line_text(m.src_line as usize);
                let clean = t.trim().trim_start_matches('#').trim_start();
                out.push((m.src_line as usize, level, SharedString::from(clean)));
            }
        }
        *self.toc_cache.borrow_mut() = Some((self.content_gen.get(), out.clone()));
        out
    }
    /// 设置初始内容。
    pub fn with_text(mut self, text: &str) -> Self {
        self.set_source_text(text);
        self
    }

    /// 当前完整源文本（宿主写回用；rope 单真源派生）。
    pub fn text(&self) -> SharedString {
        self.doc.text_shared()
    }

    /// 显示语义行数（`str::lines()` 口径，与状态栏一致）——增量索引，O(1)，不物化全文。
    pub fn line_count(&self) -> usize {
        self.doc.line_count().max(1)
    }

    /// 标量字符数（`chars().count()` 口径，状态栏显示用）——增量维护，O(1)。
    pub fn char_count(&self) -> usize {
        self.doc.char_count()
    }

    /// （仅测试）`doc.text_shared` 被调次数——B 步断言 handler 查询/编辑不再物化全文。
    #[cfg(test)]
    pub(crate) fn text_shared_calls(&self) -> u64 {
        self.doc.text_shared_calls()
    }

    /// 聚焦句柄（元素命中/宿主聚焦用）。
    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 覆盖源并重建文档模型（load 路径：一次性 O(n)）。rope COW 为唯一源，不再旁挂
    /// 一份 `content` SharedString（每键整串复制的旧成本由此消掉）。
    ///
    /// 大文档（App load）不直接走这里：整篇 O(n) 解析挪到后台（[`Self::open_from_rope`]），
    /// 完成后再 [`Self::install_open`] 装配——本函数只保留给同步路径（小文档/测试）。
    pub fn set_source_text(&mut self, text: &str) {
        let ss = SharedString::from(text);
        self.install_open(Self::open_from_str(ss));
    }

    /// 是否已装入完整源（见 [`Self::loaded`]）：`false` = App 大文档后台解析在途，
    /// 编辑器内容为空，宿主读写文本必须回退源码编辑区。
    pub(crate) fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// 从 `SharedString` 完整构建（同步路径；`set_source_text` 直接用它）。
    ///
    /// 派生扫描的**标记计数器**（全篇各一次 O(n)，之后增量维护 O(编辑)）——`derived()`
    /// 预筛用计数，避免每次内容变更对 7MB 全篇 `contains('$')` 重扫（debug 下 ~100ms/键）。
    pub(crate) fn open_from_str(text: SharedString) -> WyOpenResult {
        let _t = crate::prof::begin();
        let doc = Doc::open(text.clone());
        crate::prof::end("wy.open.doc", _t);
        WyOpenResult {
            doc,
            math_dollar_cnt: text.as_ref().bytes().filter(|&b| b == b'$').count(),
            alert_marker_cnt: text.as_ref().matches("[!").count(),
        }
    }

    /// 从编辑器 `Rope`（O(1) COW 快照）完整构建——App 大文档 load 的后台段：
    /// `Doc::open` 6.4MB 解析（~200ms）与标记计数全在这里，UI 线程只做快照 + 装配。
    pub(crate) fn open_from_rope(rope: Rope) -> WyOpenResult {
        let _t = crate::prof::begin();
        let ss = SharedString::from(rope.to_string());
        crate::prof::end("wy.open.to_string", _t);
        Self::open_from_str(ss)
    }

    /// 装配后台结果（UI 线程）：整篇重活已发生（`WyOpenResult`），这里只剩换源后的
    /// 状态重置/缓存失效（除 `rebuild_block_regions` 为 O(块) 外全 O(1)/微量）。
    pub(crate) fn install_open(&mut self, r: WyOpenResult) {
        self.doc = r.doc;
        let _t = crate::prof::begin();
        self.rebuild_block_regions();
        crate::prof::end("wy.set_source.regions", _t);
        self.caret = self.caret.min(self.doc.len());
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.open_unit = None;
        self.last_edit = None;
        self.focused_mermaid = None;
        // 换源：图片编辑卡片/悬浮态/选中态随行号失效 → 关闭。
        self.image_edit = None;
        self.image_edit_anchor.set(None);
        self.hover_image.set(None);
        self.selected_image.set(None);
        *self.frame.borrow_mut() = None;
        self.content_gen.set(self.content_gen.get().wrapping_add(1));
        self.math_dollar_cnt.set(r.math_dollar_cnt);
        self.alert_marker_cnt.set(r.alert_marker_cnt);
        self.struct_ver.set(0);
        self.struct_dirty_line.set(0);
        self.struct_old_end_line.set(0);
        self.struct_line_delta.set(0);
        self.struct_blk_old.set(0);
        self.struct_blk_span_delta.set(0);
        self.struct_edit_pending.set(false);
        self.math_line_map.borrow_mut().clear();
        // 换源：行的 src_line 全变 → 行内公式→行的登记作废（重新 claim 时再登记）。
        self.inline_math_rows.borrow_mut().clear();
        // 换源：数学位图钉随行号作废（重新绘制/claim 时再录）。
        self.clear_math_pins();
        // 全新内容：派生/数学清单/基础布局全量失效（新代 → 下帧全篇重建）。
        *self.block_layout.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
        self.regions_ver.set(self.regions_ver.get().wrapping_add(1));
        *self.pending_patch_blocks.borrow_mut() = None;
        // 图片行高收录随内容/拼图失效一并清空（重建路径 pass1 已按当前图片状态重估，
        // 残留 src 反打旧文幂等但无谓；见 `patch_pending_image_heights`）。
        self.pending_image_srcs.borrow_mut().clear();
        // 换源：搜索高亮行号/内容作废（同预览换源清高亮）。
        self.clear_highlight();
        self.loaded = true;
    }
    /// 设置图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
        // 基准目录变化 → 重新解析图片尺寸（此前按旧基准解析的作废；真实尺寸由图片行
        // 首次进带惰性重读，见 `lazy_ensure_image_size`）。
        self.image_sizes.borrow_mut().clear();
        self.images_gen.set(self.images_gen.get().wrapping_add(1));
        *self.frame.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
        // 图片尺寸全量作废 → 基础布局里图片块高全部失效：强制整篇重建（pass1 按占位高
        // 重估，图片高变更走 `pending_image_srcs` 稀疏覆写，这里必须整体作废进代重建）。
        // 并清掉残留覆写收录，防旧 src 在本目录下误打。
        *self.block_layout.borrow_mut() = None;
        self.pending_image_srcs.borrow_mut().clear();
    }
    /// 解析图片源：URL / Windows 盘符绝对路径 / 已绝对路径原样返回，其余基于
    /// 基准目录拼成绝对路径（与预览 `view::MarkdownPreview::resolve_image_src` 一致）。
    fn resolve_image_src(&self, src: &str) -> String {
        let lower = src.to_lowercase();
        if lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("file://")
        {
            return src.to_string();
        }
        if src.len() >= 2 && src.as_bytes()[1] == b':' {
            return src.to_string();
        }
        if std::path::Path::new(src).is_absolute() {
            return src.to_string();
        }
        match &self.base_dir {
            Some(base) => base.join(src).to_string_lossy().to_string(),
            None => src.to_string(),
        }
    }

    /// 图片加载资源（解析后的路径/URL → gpui 加载器资源）。
    pub(crate) fn image_load_resource(&self, src: &str) -> gpui::Resource {
        let resolved = self.resolve_image_src(src);
        if resolved.starts_with("http://")
            || resolved.starts_with("https://")
            || resolved.starts_with("file://")
        {
            gpui::Resource::Uri(resolved.into())
        } else {
            gpui::Resource::Path(std::sync::Arc::<std::path::Path>::from(
                std::path::PathBuf::from(resolved),
            ))
        }
    }

    /// 打开文档后预读图片行的固有尺寸（与预览一致：本地文件按宽高比显示，
    /// 首帧即用真实行高，避免「占位 → 真实」跳动）。只读文件头，O(行数，廉价)。
    ///
    /// 从原始源码行直接取 src（`parse::parse_image_line`），不物化 [`DocRow`]——
    /// 逐块 `rows_in_block_window(i, i)` 会命中 `parse_block_window` 的「末块含尾
    /// 换行 ≠ 块数」边界（debug 断言），且为读个 src 物化整行纯属浪费。
    /// A-4 冷开惰性化：图片行**首次进带 shape** 时按需读一次真实尺寸并入缓存。
    /// 替代旧 `preseed_image_sizes`（set_source_text/set_base_dir 时同步读**全篇**图片
    /// 尺寸——6.4MB/3456 图参考文档 ~130ms 阻塞首帧；且 O(图片数) 随文档图片量线性
    /// 膨胀）。`image_sizes` 是持久缓存 → 之后进带 O(1)；HTTP/已达尺寸/读取失败均短路，
    /// paint 的异步 asset 加载照常（带内图尺寸已知后只画、不重复校正）。
    pub(crate) fn lazy_ensure_image_size(&self, src: &str) {
        if self.image_sizes.borrow().get(src).is_some() {
            return;
        }
        let resolved = self.resolve_image_src(src);
        if !resolved.starts_with("http://")
            && !resolved.starts_with("https://")
            && !resolved.starts_with("file://")
            && let Some((w, h)) =
                crate::markdown::view::dimensions::image_dimensions(std::path::Path::new(&resolved))
        {
            self.image_sizes
                .borrow_mut()
                .insert(src.into(), Some((w as f32, h as f32)));
        }
    }

    /// （仅测试）整篇源文本（O(n) 复制）。产线元素访问一律走 [`Self::line_text`] / doc。
    #[cfg(test)]
    pub(crate) fn content(&self) -> String {
        self.doc.text_shared().to_string()
    }
    /// 第 `line` 行的源码文本（O(行长)；`line_str` 同源）。
    pub(crate) fn line_text(&self, line: usize) -> String {
        self.doc.line_str(line)
    }
    pub(crate) fn doc(&self) -> &Doc {
        &self.doc
    }
    pub(crate) fn caret(&self) -> usize {
        self.caret
    }
    /// 全文字节长（O(1)，rope 单真源）。
    pub(crate) fn len(&self) -> usize {
        self.doc.len()
    }

    /// 直接放置光标（源字节偏移）。命中/测试/编程式定位共用。
    pub(crate) fn set_caret(&mut self, off: usize) {
        self.caret = off.min(self.len());
        self.sel_anchor = None;
        // 光标移动 → 打断撤销合并组（同 editor/undo.rs 的 cooperative=false）。
        self.last_edit = None;
        *self.frame.borrow_mut() = None;
    }
    /* ---------- 选区 ---------- */

    /// 当前选区（归一化源字节区间）；无选区或零宽返回 `None`。
    pub(crate) fn selection(&self) -> Option<Range<usize>> {
        match self.sel_anchor {
            Some(a) if a != self.caret => {
                let s = a.min(self.caret).min(self.len());
                let e = a.max(self.caret).min(self.len());
                if s < e { Some(s..e) } else { None }
            }
            _ => None,
        }
    }

    /// 选中的源文本（复制/Ctrl+C）。
    pub(crate) fn selected_text(&self) -> String {
        self.selection()
            .map(|r| self.doc.slice_str(r))
            .unwrap_or_default()
    }

    /// 鼠标按下：锚点 = 光标处（开始拖选）。
    pub(crate) fn begin_selection_at(&mut self, off: usize) {
        let off = off.min(self.len());
        self.caret = off;
        self.sel_anchor = Some(off);
        self.dragging = true;
        *self.frame.borrow_mut() = None;
    }

    /// 拖拽中延伸选区（活动端跟随鼠标）。
    pub(crate) fn extend_selection_to(&mut self, off: usize) {
        if !self.dragging {
            return;
        }
        let off = off.min(self.len());
        self.caret = off;
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(off);
        }
        *self.frame.borrow_mut() = None;
    }

    /// 鼠标松开结束拖选。
    pub(crate) fn end_selection(&mut self) {
        self.dragging = false;
        // 零宽（纯点击）→ 视为无选区。
        if self.sel_anchor == Some(self.caret) {
            self.sel_anchor = None;
        }
        *self.frame.borrow_mut() = None;
    }

    /// 全选。
    pub(crate) fn select_all(&mut self) {
        self.sel_anchor = Some(0);
        self.caret = self.len();
        *self.frame.borrow_mut() = None;
    }

    /* ---------- 搜索跳转高亮（预览独有能力，搬入统一组件） ---------- */

    /// 搜索跳转后高亮「源码第 `line` 行」（0 基）内**第一个命中 `query` 的文本**，
    /// 只给搜索词本身着色（不吃整行），[`crate::config::timing::HIGHLIGHT_MS`] 后
    /// 自动消失（与 `MarkdownPreview::highlight_search` 同款语义）。对小程序否则
    /// 仅登记状态，paint 逐个源区间在用搜索色调绘制（复用选区几何）。
    pub fn highlight_search(&mut self, line: usize, query: &str, cx: &mut Context<Self>) {
        self.highlight_search = Some((line, query.to_lowercase()));
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        let seq = self.highlight_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    crate::config::timing::HIGHLIGHT_MS,
                ))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.highlight_seq == seq {
                    view.highlight_search = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 清除搜索跳转高亮。
    pub fn clear_highlight(&mut self) {
        self.highlight_search = None;
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
    }

    /// 解析搜索高亮为**绝对源字节区间**（命中词在 `line` 源码内首次出现的字节范围）。
    /// 无高亮 / 行内无命中返回 `None`。paint 与测试共用（区间语义同选区）。
    pub(crate) fn search_highlight_range(&self) -> Option<std::ops::Range<usize>> {
        let (line, query) = self.highlight_search.as_ref()?;
        let text = self.line_text(*line);
        let range = crate::markdown::view::find_text_range(&text, query)?;
        Some(self.line_start(*line) + range.start..self.line_start(*line) + range.end)
    }

    /// 复制：写入平台剪贴板（由 `on_action(Copy)` 装配）。
    pub(crate) fn copy(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.selected_text()));
    }

    /// 剪切：复制 + 删除选区。
    pub(crate) fn cut(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("cut");
        self.copy(window, cx);
        let range = self.selection().unwrap_or(self.caret..self.caret);
        self.replace_range(range, "", cx);
    }

    /// 粘贴：用剪贴板文本替换选区（若为空则插入光标处）。
    pub(crate) fn paste(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("paste");
        let text = cx
            .read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
            .unwrap_or_default();
        let range = self.selection().unwrap_or(self.caret..self.caret);
        if text.is_empty() && range.start == range.end {
            return;
        }
        self.replace_range(range, &text, cx);
    }

    /// 直输/提交（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked，
    /// 再清掉 marked——否则下一次组合会把已提交文本当旧 marked 再次替换。
    pub(crate) fn replace_text_utf16(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        // A1 只读门控：直输短路（`replace_range` 亦硬门控；这里提前返回省一次 op 账）。
        if !self.editable {
            return;
        }
        crate::prof::op_start("type");
        let r = match range {
            // 显式 UTF-16 区间（macOS/部分平台）走行粒度索引换算，O(log n + 行长)。
            Some(r) => self.doc.utf16_to_byte(r.start)..self.doc.utf16_to_byte(r.end),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection().unwrap_or(self.caret..self.caret)),
        };
        self.replace_range(r, text, cx);
        self.marked = None;
    }

    /// IME 组合更新（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked
    /// （没有 marked 时才落普通选区/光标）——否则 Windows 每次组合更新都不带区间，
    /// 新旧组合文本会一前一后堆叠出「nnini'hni'hao你好」式垃圾串。
    pub(crate) fn replace_and_mark_utf16(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        // A1 只读门控：IME 组合短路（不置 marked、不改文本）。
        if !self.editable {
            return;
        }
        crate::prof::op_start("ime");
        let byte_range = match range {
            Some(r) => self.doc.utf16_to_byte(r.start)..self.doc.utf16_to_byte(r.end),
            None => self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection().unwrap_or(self.caret..self.caret)),
        };
        self.replace_range(byte_range.clone(), new_text, cx);
        if new_text.is_empty() {
            // 组合被取消：清掉 marked，光标回组合起点。
            self.marked = None;
            self.caret = byte_range.start.min(self.len());
        } else {
            self.marked = Some(
                byte_range.start
                    ..byte_range.start + new_text.len().min(self.len() - byte_range.start),
            );
            // `new_selected_range` 是**相对 new_text 起点**的 UTF-16 偏移，不是文档
            // 绝对偏移（AppKit setMarkedText 的 selectedRange 语义）。
            let caret = match new_selected_range {
                Some(sel) => {
                    byte_range.start
                        + input::offset_from_utf16(new_text, sel.start).min(new_text.len())
                }
                None => byte_range.start + new_text.len(),
            };
            self.caret = caret.min(self.len());
        }
        cx.notify();
    }
}
