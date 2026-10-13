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
            epub: false,
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
            prebuild_slot: RefCell::new(None),
            struct_ver: Cell::new(0),
            struct_dirty_line: Cell::new(0),
            struct_old_end_line: Cell::new(0),
            struct_line_delta: Cell::new(0),
            struct_blk_old: Cell::new(0),
            struct_blk_span_delta: Cell::new(0),
            struct_edit_pending: Cell::new(false),
            regions_ver: Cell::new(0),
            regions_shift_ver: Cell::new(0),
            fold_ver: Cell::new(0),
            fold_plan: RefCell::new(None),
            fp_sets: RefCell::new(None),
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
            memory_images: std::collections::HashMap::new(),
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
            last_edit_at: None,
            live_ops: None,
            hover_image: Cell::new(None),
            selected_image: Cell::new(None),
            image_edit: None,
            image_edit_anchor: Cell::new(None),
            math_edit: None,
            math_edit_anchor: Cell::new(None),
            math_edit_sub: None,
            math_edit_drag: Cell::new(None),
            math_edit_delta: Cell::new(point(px(0.), px(0.))),
            note_ctx: std::collections::BTreeMap::new(),
            hover_note: Cell::new(None),
            marker_hl: Cell::new(None),
            marker_hl_seq: Cell::new(0),
            note_down: Cell::new(None),
            note_down_seq: Cell::new(0),
            cit: RefCell::new(None),
        }
    }

    /// 覆盖主题。
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
            // 只读：数学编辑卡（写回依赖可编辑）一并收起 + 订阅 drop + 拖拽态清空。
            self.math_edit = None;
            self.math_edit_anchor.set(None);
            self.math_edit_sub = None;
            self.math_edit_drag.set(None);
            self.math_edit_delta.set(point(px(0.), px(0.)));
        }
        *self.frame.borrow_mut() = None;
    }

    /// 当前是否可编辑（只读预览 = `false`）。
    pub fn is_editable(&self) -> bool {
        self.editable
    }

    /// 切 EPUB 阅读模式（EPUB 只读宿主）：标题默认居中（对齐 A 侧
    /// `MarkdownPreview::set_epub` 排版语义），带 `%%ALIGN:X%%` 指令的行以指令为准。
    /// 纯视图态，不进撤销栈；切换自增内容代使下帧全量重排。
    ///
    /// **M5-B5 P4-2 弃用决策（首行缩进 / float）**：A 侧 `MarkdownPreview` 的
    /// `set_epub` 还启用了「正文首行缩进 2 字符」与 `%%FLOAT%%`（图/引文左右浮动）
    /// 两项 EPUB 排版。统一组件（WYSIWYG）**未迁入**：
    ///
    /// - 首行缩进是**段落级排版指令**，WYSIWYG 的 DocRow 按行整形、无段落语义；为它加
    ///   段落首行属性需动 `doc`/`shape` 两管线，成本高、收益低（绝大多数 EPUB 正文段落
    ///   本身已是缩进/空格的源码表示）。
    /// - `%%FLOAT%%` 依赖「块占位 → 环绕重排版」，WYSIWYG 块高确定性量程（Fenwick）
    ///   不支持环绕；A 侧 float 也只对单图右留白生效，语义残缺。
    ///
    /// 接受范围：EPUB 首行缩进退化为自然换行、float 图片退化为独立图片行（仍在
    /// `%%ALIGN%%` 左右对齐控制内）；回归矩阵（蓝图 §4）已按此放宽「EPUB 排版差异」项。
    pub fn set_epub(&mut self, on: bool) {
        if self.epub == on {
            return;
        }
        self.epub = on;
        *self.frame.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
    }

    /// 当前是否 EPUB 阅读模式（只读 EPUB 宿主）。
    pub(crate) fn is_epub(&self) -> bool {
        self.epub
    }

    /// EPUB 只读宿主的轻量视口分页读数 `(当前页, 总页数)`（1 基，P4-1 迁入
    /// `MarkdownPreview::viewport_page_bounds` 语义，状态栏页码同口径）：
    /// 量程源 = WYSIWYG `Frame` 的 Fenwick **确定性总高**（比预览抽样模型更准），
    /// 滚动顶部 = 宿主每帧推入的 `viewport_top_px` + 顶部内衬换算。非 EPUB /
    /// 帧未就绪 → `None`。`view_h` = 宿主视口高（px，调用方按实际容器传）。
    /// 与 `MarkdownPreview` 同语义：当前页以视口**底部到达**的页计（拖到底 = N/N）。
    pub(crate) fn viewport_page_bounds(&self, view_h: f32) -> Option<(usize, usize)> {
        if !self.epub {
            return None;
        }
        // 顶部内衬：`app::panes::LIVE_BODY_PAD_Y` 同值（宿主 `#live-body` py；内容高含
        // 上下内衬）。若改宿主内衬必须同步这里（分页边界一致性）。
        const HOST_PAD_Y: f32 = 8.0;
        let total = self.frame.borrow().as_ref().map(|f| f32::from(f.total))?;
        let content_h = (total + 2. * HOST_PAD_Y).max(1.);
        let view_h = view_h.max(16.);
        // 视口顶：宿主每帧推入的 `viewport_top_px`（已 `max(0.)`，内容顶起点 = 0）。
        let top_px = self.viewport_top_px.get().max(0.);
        let total_pages = (content_h / view_h).ceil().max(1.) as usize;
        let bottom = top_px + view_h;
        let page = ((bottom - 0.5) / view_h).floor().max(0.) as usize;
        Some((page.min(total_pages - 1) + 1, total_pages))
    }

    /// 注册内存图片表（虚拟 src → 图片字节，不落盘；EPUB 用 `epubimg://…`）。
    /// 覆盖旧表；图片尺寸/整形/基础布局随之失效（下帧按新图重排）。
    pub fn set_memory_images(
        &mut self,
        images: std::collections::HashMap<String, std::sync::Arc<gpui::Image>>,
    ) {
        self.memory_images = images;
        self.image_sizes.borrow_mut().clear();
        self.images_gen.set(self.images_gen.get().wrapping_add(1));
        // 内存图尺寸全量作废 → 基础布局里图片块高全部失效：强制整篇重建。
        *self.block_layout.borrow_mut() = None;
        *self.frame.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
        self.pending_image_srcs.borrow_mut().clear();
    }

    /// 查内存图片表（EPUB 虚拟 src → 字节）；未登记返回 `None`（走普通路径加载）。
    pub(crate) fn memory_image(
        &self,
        src: &str,
    ) -> Option<std::sync::Arc<gpui::Image>> {
        self.memory_images.get(src).cloned()
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

    /// （仅测试/诊断）当前帧确定性总高（px；Fenwick 量程真源，同 `viewport_page_bounds`
    /// 口径）。帧未就绪 → `None`。
    pub(crate) fn frame_total(&self) -> Option<f32> {
        self.frame.borrow().as_ref().map(|f| f32::from(f.total))
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
            prebuild: None,
        }
    }

    /// 从编辑器 `Rope`（O(1) COW 快照）完整构建——App 大文档 load 的后台段：
    /// `Doc::open_rope` 零整份拷贝（blocks-only 跳过 toc）与标记计数全在这里，
    /// UI 线程只做快照 + 装配。不再 `rope.to_string()` 物化全文（曾 6.4MB 整份拷）。
    pub(crate) fn open_from_rope(rope: Rope) -> WyOpenResult {
        let _t = crate::prof::begin();
        let (math_dollar_cnt, alert_marker_cnt) = Self::count_marks(&rope);
        crate::prof::end("wy.open.marks", _t);
        let _t = crate::prof::begin();
        let doc = Doc::open_rope(rope);
        crate::prof::end("wy.open.doc", _t);
        // Step4-①：解析重叠期预建首帧 Pass1 输入（后台线程，无窗口/主题——只算
        // doc 依赖量；首帧以真实主题/宽度纯算术物化，见 `frame::pass1_from_prebuild`）。
        let prebuild = crate::markdown::wysiwyg::frame::build_prebuild_pass1(&doc);
        WyOpenResult {
            doc,
            math_dollar_cnt,
            alert_marker_cnt,
            prebuild: Some(prebuild),
        }
    }

    /// 从既有 `Doc` **克隆共享**装配（App toggle 复用路径）：右栏 `md_wy` 已解析的
    /// `Doc` O(1) 克隆复用（`Doc::clone` 共享不复制），不再 `open_from_rope` 整篇重解析。
    /// 宿主（后台）对克隆只补算一次首帧预建（[`crate::markdown::wysiwyg::frame::build_prebuild_pass1`]）后调
    /// 本函数——`install_open` 语义与 `open_from_rope` 完全一致（存预建槽 + arm 冷开保温，
    /// 首帧即轻）。`math/alert` 计数克隆自只读 `md_wy`（与克隆 Doc 同源、编辑前不变式）。
    /// 留在本级（而非 App 侧直接拼 `WyOpenResult`）：字段契约/预建语义归 wysiwyg 管。
    pub(crate) fn open_result_from_doc(
        doc: Doc,
        math_dollar_cnt: usize,
        alert_marker_cnt: usize,
        prebuild: Option<PrebuildPass1>,
    ) -> WyOpenResult {
        WyOpenResult { doc, math_dollar_cnt, alert_marker_cnt, prebuild }
    }

    /// 全文标记计数（`$` / `[!`）——`derived()` 的数学/警报预筛用增量计数（内容级
    /// O(n) 一次，之后编辑逐键增量维护，避免每次变更对 6.4MB 全篇重扫）。直接在
    /// rope 字节流上单遍滑窗（跨 chunk 连续，与 `str::bytes`/`matches("[!")` 同值）
    /// ——不物化全文。
    pub(crate) fn count_marks(rope: &Rope) -> (usize, usize) {
        let mut dollar = 0usize;
        let mut alert = 0usize;
        let mut prev = 0u8;
        for b in rope.bytes() {
            if b == b'$' {
                dollar += 1;
            }
            if b == b'!' && prev == b'[' {
                alert += 1;
            }
            prev = b;
        }
        (dollar, alert)
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
        self.last_edit_at = None;
        // 整篇换源：后台加载窗口内（旧空壳上）缴存的陈旧 op 与新的源坐标不再对应，
        // 必须清空（否则切回时把它们错位重放到源码编辑区）。收集开关本身保留。
        if let Some(log) = &mut self.live_ops {
            log.clear();
        }
        self.focused_mermaid = None;
        // 换源：图片编辑卡片/悬浮态/选中态随行号失效 → 关闭。
        self.image_edit = None;
        self.image_edit_anchor.set(None);
        self.hover_image.set(None);
        self.selected_image.set(None);
        // 换源：数学编辑卡（锚定行号/正文随源失效）关闭 + 订阅 drop + 拖拽态清空。
        self.math_edit = None;
        self.math_edit_anchor.set(None);
        self.math_edit_sub = None;
        self.math_edit_drag.set(None);
        self.math_edit_delta.set(point(px(0.), px(0.)));
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
        self.regions_shift_ver.set(0);
        *self.fp_sets.borrow_mut() = None;
        *self.pending_patch_blocks.borrow_mut() = None;
        // 图片行高收录随内容/拼图失效一并清空（重建路径 pass1 已按当前图片状态重估，
        // 残留 src 反打旧文幂等但无谓；见 `patch_pending_image_heights`）。
        self.pending_image_srcs.borrow_mut().clear();
        // 换源：搜索高亮行号/内容作废（同预览换源清高亮）。
        self.clear_highlight();
        // 换源：注记悬浮/加亮作废（行号随源失效；引文索引缓存键 = content_gen，自动失效）。
        self.hover_note.set(None);
        self.marker_hl.set(None);
        // 换源：注记「单击确认」待跳转作废——换源后旧 hit 的 src_line/注记号随源失效，
        // 残留注记的确认定时器若到点会拿旧 hit 对新文档做 `navigate_marker`（引用/定义
        // 同号时误跳对端）。清 `note_down` + 推高 `note_down_seq`（旧定时器对不上代即静默），
        // 与上两行悬浮/加亮同类：打开新文档/切换标签/EPUB 重喂源后不残留跳转。
        self.note_down.set(None);
        self.note_down_seq.set(self.note_down_seq.get().wrapping_add(1));
        // Step4-①：存首帧基础布局**预建输入**槽（`build_frame` 首帧经
        // `pass1_from_prebuild` 一次性物化后清空）；同步/Str 路径为 None → 走既有 path。
        let cold_backload = r.prebuild.is_some();
        self.prebuild_slot.replace(r.prebuild);
        // Step4-③：冷开反门控——仅**后台 rope 冷开路径**（有预建 = 大文档换源）装完后
        // 强制预整形 `cold_topup` 帧。静置门控（0746946）在滚动前会把预热门关掉 →
        // 用户一滚落点带冷、Pass2 突发；冷开期每帧预算内推进直到首几带 warm，任何
        // 滚动/编辑/宽度变化由 `idle_skip` 打断回归正常门控。同步 Str 路径（测试 /
        // 小文档 `with_text`）不 arm → 既有 prewarm 单测不受影响。
        if cold_backload {
            self.prewarm.borrow_mut().arm_cold_topup(
                crate::markdown::wysiwyg::Prewarm::COLD_TOPUP_FRAMES,
            );
        }
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
        // EPUB 内存图（`epubimg://…` 虚拟 src）：直接读字节固有尺寸，不读文件不落盘。
        if let Some(img) = self.memory_images.get(src)
            && let Some((w, h)) =
                crate::markdown::view::dimensions::image_dimensions_bytes(&img.bytes)
        {
            self.image_sizes
                .borrow_mut()
                .insert(src.into(), Some((w as f32, h as f32)));
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
    /// `$` 标记计数（只读；共享复用路径 App 把它原样带进装配结果）。
    pub(crate) fn math_dollar_cnt(&self) -> usize {
        self.math_dollar_cnt.get()
    }
    /// `[!` 标记计数（只读；共享复用路径 App 把它原样带进装配结果）。
    pub(crate) fn alert_marker_cnt(&self) -> usize {
        self.alert_marker_cnt.get()
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
        self.last_edit_at = None;
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

    /* ---------- 注记/引文（M5-B5 P4-2 迁自 MarkdownPreview，统一组件承载） ---------- */

    /// 注入 EPUB 注记正文表（`注记号 → 注释文本`；`set_epub(true)` 打开阅读宿主时由
    /// app 喂入）。替换旧表并清掉悬浮/加亮态（注记号可能已消失）。
    pub fn set_notes(&mut self, notes: std::collections::BTreeMap<usize, String>) {
        self.note_ctx = notes
            .into_iter()
            .map(|(k, v)| (k, SharedString::from(v)))
            .collect();
        self.hover_note.set(None);
        self.marker_hl.set(None);
    }

    /// 注记正文：EPUB 注记表优先，其次引文索引 `details`（定义 `[!n]` 行剥标记后的文本）。
    pub(crate) fn note_text_for(&self, n: usize) -> Option<SharedString> {
        if let Some(t) = self.note_ctx.get(&n) {
            return Some(t.clone());
        }
        self.citation_index().details.get(&n).cloned()
    }

    /// 引文跳转索引（懒建 + `content_gen` 缓存）：全篇每个含注记的块扫一遍（O(块数)），
    /// 「行」字段语义 = **源码行**（统一组件用 `set_scroll_target` 按源码行跳转，区别于
    /// A 侧预览的渲染行下标）。只交互到注记时才建，不打字/滚动热路径（同 A 侧
    /// `MarkdownPreview::ensure_citations` 的懒重扫语义）。
    pub(crate) fn citation_index(&self) -> std::rc::Rc<crate::markdown::parse::CitationIndex> {
        let cg = self.content_gen.get();
        if let Some((g, idx)) = &*self.cit.borrow()
            && *g == cg
        {
            return idx.clone();
        }
        let mut idx = crate::markdown::parse::CitationIndex::default();
        let n = self.doc.block_count();
        for k in 0..n {
            let Some(dr) = self.doc.rows_in_block_window(k, k + 1).into_iter().next() else {
                continue;
            };
            for (range, m) in &dr.notes {
                if range.start < range.end && range.end <= dr.text.len() {
                    idx.refs.entry(*m).or_insert((dr.src_line, range.clone()));
                }
            }
            for (range, m) in &dr.note_defs {
                if range.start < range.end && range.end <= dr.text.len() {
                    idx.defs.entry(*m).or_insert((dr.src_line, range.clone()));
                    idx.details.entry(*m).or_insert_with(|| {
                        let s = range.end.min(dr.text.len());
                        dr.text[s..].trim().into()
                    });
                }
            }
        }
        let idx = std::rc::Rc::new(idx);
        *self.cit.borrow_mut() = Some((cg, idx.clone()));
        idx
    }

    /// 点击注记标记后跳转对端（文内引用 ↔ 末尾定义双向，与 A 侧
    /// `MarkdownPreview::navigate_marker` 同语义）：文内 `[^n]` → 定义 `[!n]`；
    /// 定义 → 文内引用。对端经 `set_scroll_target` 滚动（元素 prepaint 逐帧重钉，
    /// 无需 window），落位后加亮对端标记；无对端（孤立标记）仅清点选/悬浮、不跳转。
    pub(crate) fn navigate_marker(&mut self, hit: &element::NoteMarkerHit, cx: &mut Context<Self>) {
        let idx = self.citation_index();
        let target = if hit.is_def {
            idx.refs.get(&hit.n).cloned()
        } else {
            idx.defs.get(&hit.n).cloned()
        };
        match target {
            Some((src_line, _)) => {
                self.hover_note.set(None);
                self.set_scroll_target(src_line);
                self.set_marker_highlight(src_line, hit.n, !hit.is_def, cx);
                cx.notify();
            }
            None => {
                self.hover_note.set(None);
                cx.notify();
            }
        }
    }

    /// 跳转后给目标行 `n` 注记标记加亮（`is_def` 指明定义/文内），[`HIGHLIGHT_MS`] 后
    /// 自动消失（代序号防旧定时器误清，与 `highlight_search` 同款）。
    pub(crate) fn set_marker_highlight(
        &mut self,
        src_line: usize,
        n: usize,
        is_def: bool,
        cx: &mut Context<Self>,
    ) {
        self.marker_hl.set(Some((src_line, n, is_def)));
        self.marker_hl_seq.set(self.marker_hl_seq.get().wrapping_add(1));
        let seq = self.marker_hl_seq.get();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    crate::config::timing::HIGHLIGHT_MS,
                ))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.marker_hl_seq.get() == seq {
                    view.marker_hl.set(None);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 当前悬浮命中的注记（内容列局部矩形；无 → `None`）。
    pub(crate) fn hover_note(&self) -> Option<element::NoteMarkerHit> {
        self.hover_note.get()
    }

    /// （仅测试）当前注记跳转加亮 `(源码行, 注记号, 是否定义)`（`navigate_marker` 断言）。
    #[cfg(test)]
    pub(crate) fn marker_hl_test(&self) -> Option<(usize, usize, bool)> {
        self.marker_hl.get()
    }

    /// 注记悬浮卡片浮层（有悬浮命中时返回）：`deferred(anchored(Window, TopLeft))` 锚定
    /// 在注记矩形右下（边距 8/4，对齐 A 侧 `MarkdownPreview::note_card_point`），主题
    /// popover 配色。参考 `image_edit_card_element` 的浮层约定（逃逸滚动容器裁剪/命中，
    /// 随内容滚动跟手）。正文 = EPUB 注记表，缺则回退引文 `details`；无正文不浮卡。
    pub(crate) fn note_card_element(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui::{
            Anchor, AnchoredPositionMode, Edges, InteractiveElement, IntoElement, ParentElement,
            Styled,
        };
        let hit = self.hover_note()?;
        let text = self.note_text_for(hit.n)?;
        let origin = self.content_origin();
        let at = gpui::point(
            origin.x + hit.bounds.right() + px(8.),
            origin.y + hit.bounds.top() + px(4.),
        );
        let colors = crate::ui::theme::Theme::global(cx).colors;
        let card_bg: gpui::Hsla = colors.popover_bg.into();
        let border: gpui::Hsla = colors.border.into();
        let fg: gpui::Hsla = colors.fg.into();
        let card = gpui::div()
            .debug_selector(|| "wy-note-card".into())
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded_md()
            .p_3()
            .max_w(px(460.))
            .child(
                gpui::div()
                    .whitespace_normal()
                    .text_size(px(12.5))
                    .text_color(fg)
                    .child(text),
            );
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position_mode(AnchoredPositionMode::Window)
                    .anchor(Anchor::TopLeft)
                    .position(at)
                    .snap_to_window_with_margin(Edges::all(px(8.)))
                    .child(card),
            )
            .with_priority(100)
            .into_any_element(),
        )
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

    /// 直输/提交（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked、
    /// 再清掉 marked——否则下一次组合会把已提交文本当旧 marked 再次替换。
    /// 状态机已收敛到共享 `crate::ime::replace`（mark=false：提交/直输落编辑后
    /// 因 `supports_marked` 清 marked；undo 意图由 `replace_range` 自推导）。
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
        crate::ime::replace(self, range, text, None, false, cx);
    }

    /// IME 组合更新（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked
    /// （没有 marked 时才落普通选区/光标）——否则 Windows 每次组合更新都不带区间，
    /// 新旧组合文本会一前一后堆叠出「nnini'hni'hao你好」式垃圾串。
    /// 状态机已收敛到共享 `crate::ime::replace`（mark=true：Atomic 意图落编辑 +
    /// 维护 marked + 组合内光标）；WYSIWYG 专有的「只取 `sel.start` 摆光标」姿势由
    /// `ImeTextHost::set_composition_caret` 覆写承载。末尾 `cx.notify()` 保持现状
    /// （空区间取消时 `replace_range` 短路不 notify，这里兜一帧）。
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
        crate::ime::replace(self, range, new_text, new_selected_range, true, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count_marks`（rope 字节流滑窗）与字符串口径（`bytes().filter('$')` /
    /// `matches("[!")`）逐值对拍——冷开替换旧 `open_from_str` 的全篇串扫描后，
    /// `derived()` 的数学/警报预筛计数必须不漂移。
    #[test]
    fn count_marks_matches_string_reference() {
        let samples = [
            "",
            "纯文本无标记",
            "$x + y$ 与 $z$",
            "[!NOTE] 开头 + [!WARNING]",
            "数学 $a$[!TIP] 混排 $[! $b$",
            "$$ 块级 + [!CAUTION]\n第二行 [!!] 双感叹",
            "转义 \\$cash$ 不规则",
            "emoji 😀 $🚀$ [!️!]",
        ];
        for s in &samples {
            let (d, a) = WyEditor::count_marks(&ropey::Rope::from(*s));
            let want_d = s.bytes().filter(|&b| b == b'$').count();
            let want_a = s.matches("[!").count();
            assert_eq!(d, want_d, "dollar {s:?}");
            assert_eq!(a, want_a, "alert {s:?}");
        }
        // 多叶 rope：构造 `[!` 恰好跨叶边界（首叶 1024B 以 `[` 收尾，`!` 在次叶首），
        // 验证字节流滑窗跨 chunk 不漏计（也覆盖 `$` 跨叶）。
        let long = format!("{}[!WARN]$", "a".repeat(1023));
        let rope = ropey::Rope::from(long.as_str());
        assert_eq!(rope.chunks().count() > 1, true);
        let (d, a) = WyEditor::count_marks(&rope);
        assert_eq!(d, long.bytes().filter(|&b| b == b'$').count());
        assert_eq!(a, long.matches("[!").count());
    }
}
