use super::*;

use gpui::Pixels;
use ime::ImeTextHost;
use ropey::Rope;

use crate::markdown::view::style::StyleParams;

/// 本批 Pattern 1：`WyEditor` 以 `caret_phase` 字段作为光标闪烁相位状态机宿主，
/// 供 `editor_core::caret::spawn_blink_loop` 的循环体经 `Entity` 更新访问。
impl editor_core::caret::CaretOwner for WyEditor {
    fn caret(&mut self) -> &mut editor_core::caret::CaretPhase {
        &mut self.caret_phase
    }
}

/// WyEditor 核心生命周期 / 文本 API / 选区 / 剪贴板 / IME 直输（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ts = window.text_style();
        let fs: Pixels = ts.font_size.to_pixels(window.rem_size());
        let p = StyleParams::from_theme_with_font(cx, Some(f32::from(fs)));
        Self {
            doc: Doc::open(SharedString::default()),
            focus: cx.focus_handle(),
            // 光标闪烁相位状态机（`flip_when_blurred=true`：失焦仍按定时器翻转，见 core.rs 策略）。
            caret_phase: editor_core::caret::CaretPhase::new(true),
            focus_registered: false,
            caret: 0,
            loaded: false,
            install_state: WyInstallState::default(),
            editable: true,
            epub: false,
            // 渲染模型状态全部收敛进 `WyRenderModel`（字段初值与迁移前逐字一致，见引擎
            // `model.rs::new`；含主题快照/帧/整形缓存/布局代数/图量/数学/横滚缓存）。
            rd: RefCell::new(markdown_wysiwyg_engine::WyRenderModel::new(
                WyTheme::from_style(&p, ts.font(), fs),
            )),
            last_bounds: Cell::new(None),
            zoom_anchor: RefCell::new(None),
            viewport_top_px: Cell::new(0.0),
            scroll_target: Cell::new(None),
            last_pin: Cell::new(None),
            scroll_applies: Cell::new(0),
            zoom_last_target: Cell::new(None),
            zoom_applies: Cell::new(0),
            host_scroll: RefCell::new(None),
            image_cache: gpui::RetainAllImageCache::new(cx),
            image_resize_drag: RefCell::new(None),
            math_slots: Rc::new(RefCell::new(MathSlotCache::new(
                config::limits::MATH_SLOTS_CACHE_CAP,
            ))),
            math_color: RefCell::new(None),
            focused_mermaid: None,
            code_parse: std::sync::Arc::new(std::sync::Mutex::new(CodeParseCache::new(
                config::limits::CODE_PARSE_CACHE_CAP,
            ))),
            code_parse_need: RefCell::new(Vec::new()),
            pending_math_drop: RefCell::new(Vec::new()),
            math_epoch: Cell::new(0),
            code_epoch: Cell::new(0),
            inline_math_rows: RefCell::new(std::collections::HashMap::new()),
            base_dir: None,
            memory_images: std::collections::HashMap::new(),
            marked: None,
            sel_anchor: None,
            dragging: false,
            toc_cache: RefCell::new(None),
            highlight_search: None,
            highlight_seq: 0,
            block_regions: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            open_unit: None,
            last_edit: None,
            last_edit_at: None,
            live_ops: None,
            hover_image: Cell::new(None),
            selected_image: Cell::new(None),
            hover_inline_image: Cell::new(None),
            selected_inline_image: Cell::new(None),
            image_edit: None,
            image_edit_anchor: CardAnchor::new(element::IMAGE_EDIT_CARD_W),
            math_edit: None,
            math_edit_anchor: CardAnchor::new(element::MATH_EDIT_CARD_W),
            math_edit_sub: None,
            math_edit_drag: Cell::new(None),
            math_edit_delta: Cell::new(point(px(0.), px(0.))),
            note_ctx: std::collections::BTreeMap::new(),
            hover_note: Cell::new(None),
            note_down: Cell::new(None),
            note_down_seq: Cell::new(0),
            cit: RefCell::new(None),
        }
    }

    /// 覆盖主题。
    pub fn set_theme(&mut self, theme: WyTheme) {
        self.rd.borrow_mut().theme = theme;
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        // 基础布局存的是 px 真值，主题/字号变化量纲改变 → 代置失效位（否则带外沿用
        // 旧字号的固定 px 高，行高与带内新字号不一致，滚动跳变）。
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        // 整形行内容同样依赖主题（字号/颜色/字体）→ 整体清空。
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
    }

    /// 显式设置字号（应用把全局字号同步进来；变更新帧缓存）。不读窗口 text_style，
    /// 保证 measure/prepaint 两阶段一致。字号不只影响正文/代码尺寸，标题也按
    /// `StyleParams::heading_metrics` 等比缩放（见 `WyTheme::set_font_size`）。
    pub fn set_font_size(&mut self, size: Pixels) {
        if self.rd.borrow().theme.font_size == size {
            return;
        }
        // 缩放锚点：重排前记录视口顶部内容点，帧按新字号重建后由 element prepaint
        // 自驱动滚回（`advance_zoom_restore`）——Ctrl+滚轮缩放不带动滚动位置，
        // 对齐 view/editor。
        self.capture_zoom_anchor();
        self.rd.borrow_mut().theme.set_font_size(size);
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        // 同 `set_theme`：字号变 → 基础布局（px）失效。
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        // 字号变 → 行整形（宽/高）也失效。
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
    }

    /// 从全局主题**重建整份调色主题**（应用「切换黑白主题」后调用）：WyTheme 在
    /// `new()` 时由 [`StyleParams::from_theme_with_font`] 一次性派生并缓存（主题切换
    /// 后如不重建，预览/WYSIWYG 的文字/代码块/链接等颜色会保持旧主题色）。正文字体与
    /// 字号沿用本主题既有的（创建时从 `window.text_style()` 确定性取用），只重建色板。
    /// `set_theme` 会清空帧/布局/整形缓存，下一帧按新色板重画。
    pub fn rebuild_theme_from_global(&mut self, cx: &mut gpui::App) {
        let fs = self.rd.borrow().theme.font_size;
        let body_font = self.rd.borrow().theme.font.clone();
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
            // 只读：无编辑能力 → 收起图片编辑卡片与悬浮按钮态、图片选中态（含行内图）。
            self.image_edit = None;
            self.image_edit_anchor.clear();
            self.hover_image.set(None);
            self.selected_image.set(None);
            self.hover_inline_image.set(None);
            self.selected_inline_image.set(None);
            // 只读：数学编辑卡（写回依赖可编辑）一并收起 + 订阅 drop + 拖拽态清空。
            self.math_edit = None;
            self.math_edit_anchor.clear();
            self.math_edit_sub = None;
            self.math_edit_drag.set(None);
            self.math_edit_delta.set(point(px(0.), px(0.)));
        }
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /// 当前是否可编辑（只读预览 = `false`）。
    pub fn is_editable(&self) -> bool {
        self.editable
    }

    /// 当前是否聚焦（`track_focus` 事件态，paint 门控与 shell 测试共用）。
    pub fn is_focused(&self) -> bool {
        self.caret_phase.focused
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
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
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
    pub fn viewport_page_bounds(&self, view_h: f32) -> Option<(usize, usize)> {
        if !self.epub {
            return None;
        }
        // 顶部内衬：`app::panes::LIVE_BODY_PAD_Y` 同值（宿主 `#live-body` py；内容高含
        // 上下内衬）。若改宿主内衬必须同步这里（分页边界一致性）。
        const HOST_PAD_Y: f32 = 8.0;
        let total = self.rd.borrow().frame.borrow().as_ref().map(|f| f32::from(f.total))?;
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
        self.rd.borrow_mut().image_sizes.borrow_mut().clear();
        { let rd = self.rd.borrow_mut(); rd.images_gen.set(rd.images_gen.get().wrapping_add(1)); }
        // 内存图尺寸全量作废 → 基础布局里图片块高全部失效：强制整篇重建。
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
        self.rd.borrow_mut().pending_image_srcs.borrow_mut().clear();
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
    pub fn toc_entries(&self) -> TocEntries {
        if let Some((g, t)) = &*self.toc_cache.borrow()
            && *g == self.rd.borrow().content_gen.get()
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
        *self.toc_cache.borrow_mut() = Some((self.rd.borrow().content_gen.get(), out.clone()));
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
    pub fn frame_total(&self) -> Option<f32> {
        self.rd.borrow().frame.borrow().as_ref().map(|f| f32::from(f.total))
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
        // 换源即装机：同步路径就地装好内容 → **先作废在途后台整篇装配**（推进装配代号 +
        // 清「在途」）。否则在途 async 装配（begin_install→install_open→finish_install）的
        // 旧完成回调稍后按旧代号命中，把刚同步换上的内容覆盖回去，且 `is_loading` 卡 true。
        // `invalidate_install` 文档明示只给同步改写路径：async 装配完成路径（install_open
        // 直调）从不经本函数 → 零自伤；prefix/full 两阶段共用的 old_seq 由同步换源作废。
        self.invalidate_install();
        let ss = SharedString::from(text);
        self.install_open(Self::open_from_str(ss));
    }

    /// 是否已装入完整源（见 [`Self::loaded`]）：`false` = App 大文档后台解析在途，
    /// 编辑器内容为空，宿主读写文本必须回退源码编辑区。
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// 从 `SharedString` 完整构建（同步路径；`set_source_text` 直接用它）。
    ///
    /// 派生扫描的**标记计数器**（全篇各一次 O(n)，之后增量维护 O(编辑)）——`derived()`
    /// 预筛用计数，避免每次内容变更对 7MB 全篇 `contains('$')` 重扫（debug 下 ~100ms/键）。
    pub fn open_from_str(text: SharedString) -> WyOpenResult {
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
    pub fn open_from_rope(rope: Rope) -> WyOpenResult {
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
    pub fn open_result_from_doc(
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
    pub fn install_open(&mut self, r: WyOpenResult) {
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
        // 换源：图片编辑卡片/悬浮态/选中态随行号失效 → 关闭（含行内图）。
        self.image_edit = None;
        self.image_edit_anchor.clear();
        self.hover_image.set(None);
        self.selected_image.set(None);
        self.hover_inline_image.set(None);
        self.selected_inline_image.set(None);
        // 换源：数学编辑卡（锚定行号/正文随源失效）关闭 + 订阅 drop + 拖拽态清空。
        self.math_edit = None;
        self.math_edit_anchor.clear();
        self.math_edit_sub = None;
        self.math_edit_drag.set(None);
        self.math_edit_delta.set(point(px(0.), px(0.)));
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        { let rd = self.rd.borrow_mut(); rd.content_gen.set(rd.content_gen.get().wrapping_add(1)); }
        self.rd.borrow_mut().math_dollar_cnt.set(r.math_dollar_cnt);
        self.rd.borrow_mut().alert_marker_cnt.set(r.alert_marker_cnt);
        self.rd.borrow_mut().struct_ver.set(0);
        self.rd.borrow_mut().struct_dirty_line.set(0);
        self.rd.borrow_mut().struct_old_end_line.set(0);
        self.rd.borrow_mut().struct_line_delta.set(0);
        self.rd.borrow_mut().struct_blk_old.set(0);
        self.rd.borrow_mut().struct_blk_span_delta.set(0);
        self.rd.borrow_mut().struct_edit_pending.set(false);
        self.rd.borrow_mut().math_line_map.borrow_mut().clear();
        self.rd.borrow_mut().math_line_map_warned.set(false);
        // 换源：行的 src_line 全变 → 行内公式→行的登记作废（重新 claim 时再登记）。
        self.inline_math_rows.borrow_mut().clear();
        // 换源：数学位图钉随行号作废（重新绘制/claim 时再录）。
        self.clear_math_pins();
        // 换源：数学/代码缓存的旧文档产物整批作废——否则同一标签页持续打开不同公式/
        // 代码文档时位图 + GPU 纹理只涨不降（内存 bug 根因）。
        // `math_slots` 里 `Ready` 位图的纹理先收进 `pending_math_drop`，下一帧
        // `sync_math` 用 `cx.drop_image` 统一释放（`install_open` 无 `cx`，释放延迟
        // 一帧；Arc 未 drop 前字节仍在、可重传，不丢渲染）。同时 `math_epoch` 换代：
        // 换源前已发起的后台光栅任务完成回来自检换代 → 不写新文档槽、不触发就绪。
        let drained = self.math_slots.borrow_mut().drain_all();
        if !drained.is_empty() {
            let mut pending = self.pending_math_drop.borrow_mut();
            for s in drained {
                if let MathSlot::Ready(img, _, _) = s {
                    pending.push(img);
                }
            }
        }
        self.math_epoch.set(self.math_epoch.get().wrapping_add(1));
        // 换源：代码高亮解析缓存（旧文档 runs）与其待办队列一并作废——在途后台解析
        // 任务完成回来自检 `code_epoch`（已换代 → 不写新文档缓存、不按旧 src_line
        // 作废新文档的块）。
        self.code_parse.lock().unwrap().clear();
        self.code_epoch.set(self.code_epoch.get().wrapping_add(1));
        self.code_parse_need.borrow_mut().clear();
        // 全新内容：派生/数学清单/基础布局全量失效（新代 → 下帧全篇重建）。
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
        { let rd = self.rd.borrow_mut(); rd.regions_ver.set(rd.regions_ver.get().wrapping_add(1)); }
        self.rd.borrow_mut().regions_shift_ver.set(0);
        *self.rd.borrow_mut().fp_sets.borrow_mut() = None;
        *self.rd.borrow_mut().pending_patch_blocks.borrow_mut() = None;
        // 图片行高收录随内容/拼图失效一并清空（重建路径 pass1 已按当前图片状态重估，
        // 残留 src 反打旧文幂等但无谓；见 `patch_pending_image_heights`）。
        self.rd.borrow_mut().pending_image_srcs.borrow_mut().clear();
        // 换源：搜索高亮行号/内容作废（同预览换源清高亮）。
        self.clear_highlight();
        // 换源：注记悬浮/加亮作废（行号随源失效；引文索引缓存键 = content_gen，自动失效）。
        self.hover_note.set(None);
        self.rd.borrow_mut().marker_hl.set(None);
        // 换源：注记「单击确认」待跳转作废——换源后旧 hit 的 src_line/注记号随源失效，
        // 残留注记的确认定时器若到点会拿旧 hit 对新文档做 `navigate_marker`（引用/定义
        // 同号时误跳对端）。清 `note_down` + 推高 `note_down_seq`（旧定时器对不上代即静默），
        // 与上两行悬浮/加亮同类：打开新文档/切换标签/EPUB 重喂源后不残留跳转。
        self.note_down.set(None);
        self.note_down_seq.set(self.note_down_seq.get().wrapping_add(1));
        // Step4-①：存首帧基础布局**预建输入**槽（`build_frame` 首帧经
        // `pass1_from_prebuild` 一次性物化后清空）；同步/Str 路径为 None → 走既有 path。
        let cold_backload = r.prebuild.is_some();
        self.rd.borrow_mut().prebuild_slot.replace(r.prebuild);
        // Step4-③：冷开反门控——仅**后台 rope 冷开路径**（有预建 = 大文档换源）装完后
        // 强制预整形 `cold_topup` 帧。静置门控（0746946）在滚动前会把预热门关掉 →
        // 用户一滚落点带冷、Pass2 突发；冷开期每帧预算内推进直到首几带 warm，任何
        // 滚动/编辑/宽度变化由 `idle_skip` 打断回归正常门控。同步 Str 路径（测试 /
        // 小文档 `with_text`）不 arm → 既有 prewarm 单测不受影响。
        if cold_backload {
            self.rd.borrow_mut().prewarm.borrow_mut().arm_cold_topup(
                crate::markdown::wysiwyg::Prewarm::COLD_TOPUP_FRAMES,
            );
        }
        self.loaded = true;
    }

    /// 新装配代号（「装配代号」单调计数器收敛进 WyEditor）：任何**发起整篇装配 / 作废
    /// 在途**的路径经它取新代号并推进 [`Self::install_state`].seq——旧代号的在途回调查
    /// [`Self::begin_install`]/[`Self::finish_install`]/[`Self::fail_install`] 时按
    /// `seq < self.seq` 判陈旧丢弃（语义 = 原宿主 `md_wy_install.seq += 1`）。
    pub fn next_install_seq(&mut self) -> usize {
        self.install_state.seq = self.install_state.seq.wrapping_add(1);
        self.install_state.seq
    }

    /// 占位 / 校验一次整篇装配（宿主 async 装配完成回调先调它做**陈旧世序检查**）。
    /// 返回 `false` = 本代号已过期（`seq < self.seq`）→ 调用方应丢弃装配结果（不装旧）；
    /// `true` = 仍是最新代号 → 置「在途」并记录代号，调用方可继续 `install_open` +
    /// [`Self::finish_install`]。（代号单调前进，`seq == self.seq` 恒为最新；已换代判断
    /// 只需一次 `<` 比较——见 [`Self::next_install_seq`]。）
    pub fn begin_install(&mut self, seq: usize) -> bool {
        if seq < self.install_state.seq {
            return false; // 已换代：更晚的装配已发起
        }
        self.install_state.loading = true;
        self.install_state.install_error = None;
        self.install_state.seq = seq;
        true
    }

    /// 整篇装配成功收尾：`seq < self.seq`（已换代）→ no-op 丢弃陈旧回调；否则清「在途」+
    /// 失败信息、置 [`Self::loaded`]（≈ 原宿主同步 `install_open` 后清 `pending`）。
    pub fn finish_install(&mut self, seq: usize) {
        if seq < self.install_state.seq {
            return; // 已换代：陈旧完成回调，丢弃
        }
        self.install_state.loading = false;
        self.loaded = true;
        self.install_state.install_error = None;
    }

    /// 整篇装配失败收尾：`seq < self.seq`（已换代）→ no-op 丢弃陈旧回调；否则清「在途」、
    /// 记失败信息、**保持 [`Self::loaded`] 原样**（失败时保留已装内容/占位，不谎报就绪）。
    pub fn fail_install(&mut self, seq: usize, err: impl Into<String>) {
        if seq < self.install_state.seq {
            return; // 已换代：陈旧失败回调，丢弃
        }
        self.install_state.loading = false;
        self.install_state.install_error = Some(err.into());
    }

    /// 是否「后台整篇装配在途」（宿主门控用：替代原宿主 `md_wy_install.pending.is_some()`）。
    pub fn is_loading(&self) -> bool {
        self.install_state.loading
    }

    /// 最近一次装配失败的宿主可读信息（[`Self::fail_install`] 写入；成功/新装配清除）。
    /// 预留的对外可读面（本轮宿主走「早装失败→Rope 即时重喂」路径会把错误染色成一次
    /// 新装配并清除，故暂无不读轻体现在消费方）；供后续把失败回显到占位层用。
    pub fn install_error(&self) -> Option<&str> {
        self.install_state.install_error.as_deref()
    }

    /// 作废在途装配（宿主任何**同步改写内容**的路径在改动前调用，替代原宿主
    /// `md_wy_invalidate_async` 的 `seq += 1` + 清 `pending`）：推进代号使旧回调查陈旧，
    /// 并清「在途」与失败信息——同步 `install_open`/增量重放将接管内容，在途后台任务被丢弃。
    pub fn invalidate_install(&mut self) {
        self.install_state.seq = self.install_state.seq.wrapping_add(1);
        self.install_state.loading = false;
        self.install_state.install_error = None;
    }

    /// 设置图片相对路径解析的基准目录（打开文件时由应用传入其父目录，与预览一致）。
    pub fn set_base_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.base_dir = dir;
        // 基准目录变化 → 重新解析图片尺寸（此前按旧基准解析的作废；真实尺寸由图片行
        // 首次进带惰性重读，见 `lazy_ensure_image_size`）。
        self.rd.borrow_mut().image_sizes.borrow_mut().clear();
        { let rd = self.rd.borrow_mut(); rd.images_gen.set(rd.images_gen.get().wrapping_add(1)); }
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
        // 图片尺寸全量作废 → 基础布局里图片块高全部失效：强制整篇重建（pass1 按占位高
        // 重估，图片高变更走 `pending_image_srcs` 稀疏覆写，这里必须整体作废进代重建）。
        // 并清掉残留覆写收录，防旧 src 在本目录下误打。
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        self.rd.borrow_mut().pending_image_srcs.borrow_mut().clear();
    }
    /// 解析图片源：URL / Windows 盘符绝对路径 / 已绝对路径原样返回，其余基于
    /// 基准目录拼成绝对路径（与预览 `view::MarkdownPreview::resolve_image_src` 一致）。
    /// `pub(crate)`：`host_impl` 的 `WyRenderHost::image_intrinsic_size`（只读探测）经它复用。
    pub(crate) fn resolve_image_src(&self, src: &str) -> String {
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
    /// 仅供宿主测试消费的旧签名转发（生产无调用，EPUB 真机路径经引擎侧承担）→ allow 压
    /// dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn lazy_ensure_image_size(&self, src: &str) {
        if self.rd.borrow().image_sizes.borrow().get(src).is_some() {
            return;
        }
        // EPUB 内存图（`epubimg://…` 虚拟 src）：直接读字节固有尺寸，不读文件不落盘。
        if let Some(img) = self.memory_images.get(src)
            && let Some((w, h)) =
                crate::markdown::view::dimensions::image_dimensions_bytes(&img.bytes)
        {
            self.rd.borrow_mut().image_sizes
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
            self.rd.borrow_mut().image_sizes
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
    pub fn doc(&self) -> &Doc {
        &self.doc
    }
    /// `$` 标记计数（只读；共享复用路径 App 把它原样带进装配结果）。
    pub fn math_dollar_cnt(&self) -> usize {
        self.rd.borrow().math_dollar_cnt.get()
    }
    /// `[!` 标记计数（只读；共享复用路径 App 把它原样带进装配结果）。
    pub fn alert_marker_cnt(&self) -> usize {
        self.rd.borrow().alert_marker_cnt.get()
    }
    /// 当前光标（源字节偏移；只读，host/tests/状态栏共用）。
    pub fn caret(&self) -> usize {
        self.caret
    }
    /// 全文字节长（O(1)，rope 单真源）。
    pub fn len(&self) -> usize {
        self.doc.len()
    }

    /// 直接放置光标（源字节偏移）。命中/测试/编程式定位共用。
    pub fn set_caret(&mut self, off: usize) {
        self.caret = off.min(self.len());
        self.sel_anchor = None;
        // 光标移动 → 打断撤销合并组（同 editor/undo.rs 的 cooperative=false）。
        self.last_edit = None;
        self.last_edit_at = None;
        // 光标移动 → 静止期常亮（blink 暂停，先亮住再继续闪）。
        self.caret_phase.kick();
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /// live2 容器「内容下方空白」点击转发（场景 B）：光标钳到内容末尾 + 聚焦 +
    /// 静止期常亮——等效于用户点文档末尾后。不置选区、不开拖选（空白点击不是拖选
    /// 起点，避免 `dragging` 残留：拖到元素外松开由 `on_window_mouse_up` 兜底，但
    /// 这里不在容器路径引入拖选态）。调用方（shell 容器 `on_mouse_down`）随后 notify。
    pub fn move_caret_to_doc_end_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.set_caret(self.len());
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
        // 点击落光标 → 静止期常亮（对齐 editor `on_mouse_down` 的 kick）。
        self.caret_phase.kick();
        self.dragging = true;
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /// 拖拽中延伸选区（活动端跟随鼠标）。
    pub(crate) fn extend_selection_to(&mut self, off: usize) {
        if !self.dragging {
            return;
        }
        let off = off.min(self.len());
        self.caret = off;
        // 拖选移动光标 → 静止期常亮（对齐 editor `on_mouse_move` 拖选期实时 kick）。
        self.caret_phase.kick();
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(off);
        }
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /// 鼠标松开结束拖选。
    pub fn end_selection(&mut self) {
        self.dragging = false;
        // 零宽（纯点击）→ 视为无选区。
        if self.sel_anchor == Some(self.caret) {
            self.sel_anchor = None;
        }
        // 拖选结束 → 静止期常亮。
        self.caret_phase.kick();
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /// 全选。
    pub(crate) fn select_all(&mut self) {
        self.sel_anchor = Some(0);
        self.caret = self.len();
        self.caret_phase.kick();
        *self.rd.borrow_mut().frame.borrow_mut() = None;
    }

    /* ---------- 光标闪烁（对齐编辑器 `CodeEditor` / ui 输入框同一模式） ---------- */
    // 本批 Pattern 1：状态字段 + 常量 + 循环 + kick + caret_shown_now + 翻转/keep
    // 判定全部下沉 `editor_core::caret::CaretPhase`（收敛自 editor/interaction.rs 与
    // 本文件两份同构实现）。这里只留薄调用 + 两处语义差异策略行。

    fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.caret_phase.focused = true;
        self.caret_phase.kick();
        if !self.caret_phase.blink_running {
            self.start_blink(cx);
        }
        cx.notify();
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.caret_phase.focused = false;
        // 语义差异 2（见 editor_core::caret 模块头）：wysiwyg 失焦显式让光标常亮
        // （editor on_blur 不清 caret_visible）——该策略行留在此处，不并入状态机。
        self.caret_phase.caret_visible = true;
        cx.notify();
    }

    /// 薄调用：这里只在 `&mut self` 上下文**同步**置位 + 起循环（`begin_blink`
    /// 返回本拍代号给循环体做 keep 判定；异步循环本体在 `editor_core::caret`）。
    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.caret_phase.begin_blink() {
            editor_core::caret::spawn_blink_loop::<Self>(epoch, cx);
        }
    }

    /// 首次渲染注册焦点事件 + 每帧按窗口焦点态对齐（补应用启动时焦点先于注册的
    /// 丢失事件；聚焦/失焦都经 on_focus/on_blur 收敛到 blink 循环）。由元素
    /// `request_layout`/`prepaint` 每帧调用。
    pub(crate) fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_registered {
            let _ = cx.on_focus(&self.focus, window, Self::on_focus);
            let _ = cx.on_blur(&self.focus, window, Self::on_blur);
            self.focus_registered = true;
        }
        let focused = self.focus.is_focused(window);
        if focused && !self.caret_phase.focused {
            self.on_focus(window, cx);
        } else if !focused && self.caret_phase.focused {
            self.on_blur(window, cx);
        }
    }

    /* ---------- 搜索跳转高亮（预览独有能力，搬入统一组件） ---------- */

    /// 搜索跳转后高亮「源码第 `line` 行」（0 基）内**第一个命中 `query` 的文本**，
    /// 只给搜索词本身着色（不吃整行），[`config::timing::HIGHLIGHT_MS`] 后
    /// 自动消失（与 `MarkdownPreview::highlight_search` 同款语义）。对小程序否则
    /// 仅登记状态，paint 逐个源区间在用搜索色调绘制（复用选区几何）。
    pub fn highlight_search(&mut self, line: usize, query: &str, cx: &mut Context<Self>) {
        self.highlight_search = Some((line, query.to_lowercase()));
        self.highlight_seq = self.highlight_seq.wrapping_add(1);
        let seq = self.highlight_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    config::timing::HIGHLIGHT_MS,
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
        self.rd.borrow_mut().marker_hl.set(None);
    }

    /// 注记正文：EPUB 注记表优先，其次引文索引 `details`（定义 `[!n]` 行剥标记后的文本）。
    pub fn note_text_for(&self, n: usize) -> Option<SharedString> {
        if let Some(t) = self.note_ctx.get(&n) {
            return Some(t.clone());
        }
        self.citation_index().details.get(&n).cloned()
    }

    /// 引文跳转索引（懒建 + `content_gen` 缓存）：全篇每个含注记的块扫一遍（O(块数)），
    /// 「行」字段语义 = **源码行**（统一组件用 `set_scroll_target` 按源码行跳转，区别于
    /// A 侧预览的渲染行下标）。只交互到注记时才建，不打字/滚动热路径（同 A 侧
    /// `MarkdownPreview::ensure_citations` 的懒重扫语义）。
    pub fn citation_index(&self) -> std::rc::Rc<crate::markdown::parse::CitationIndex> {
        let cg = self.rd.borrow().content_gen.get();
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
    pub fn navigate_marker(&mut self, hit: &element::NoteMarkerHit, cx: &mut Context<Self>) {
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
        self.rd.borrow_mut().marker_hl.set(Some((src_line, n, is_def)));
        { let rd = self.rd.borrow_mut(); rd.marker_hl_seq.set(rd.marker_hl_seq.get().wrapping_add(1)); }
        let seq = self.rd.borrow().marker_hl_seq.get();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    config::timing::HIGHLIGHT_MS,
                ))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.rd.borrow().marker_hl_seq.get() == seq {
                    view.rd.borrow_mut().marker_hl.set(None);
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

    /// （测试探针）当前注记跳转加亮 `(源码行, 注记号, 是否定义)`（`navigate_marker` 断言）。
    pub fn marker_hl_test(&self) -> Option<(usize, usize, bool)> {
        self.rd.borrow().marker_hl.get()
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
            InteractiveElement, ParentElement, Styled,
        };
        let hit = self.hover_note()?;
        let text = self.note_text_for(hit.n)?;
        let origin = self.content_origin();
        let at = gpui::point(
            origin.x + hit.bounds.right() + px(8.),
            origin.y + hit.bounds.top() + px(4.),
        );
        let colors = ui::theme::Theme::global(cx).colors;
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
        Some(ui::float_card::anchor_layer(at, card))
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
    /// 数学编辑卡内嵌编辑器聚焦时（`paste_into_focused_math_edit`）先把剪贴板路由给它
    /// ——否则焦点在卡片上按 Ctrl+V 会把内容误粘到文档正文（P10 实测）。
    /// 图片编辑卡（链接/文本输入框）聚焦时同理先路由到卡片输入框（`paste_into_focused_image_edit`）。
    pub(crate) fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable {
            return;
        }
        crate::prof::op_start("paste");
        let text = cx
            .read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
            .unwrap_or_default();
        if self.paste_into_focused_image_edit(window, &text, cx) {
            return;
        }
        if self.paste_into_focused_math_edit(window, &text, cx) {
            return;
        }
        let range = self.selection().unwrap_or(self.caret..self.caret);
        if text.is_empty() && range.start == range.end {
            return;
        }
        self.replace_range(range, &text, cx);
    }

    /// 直输/提交（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked、
    /// 再清掉 marked——否则下一次组合会把已提交文本当旧 marked 再次替换。
    /// 状态机已收敛到共享 `ime::replace`（mark=false：提交/直输落编辑后
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
        // S2 标题标记引导区空格钳制（仅键入路径）。平台直输（含空格键）走这里；
        // 粘贴/撤销/其它路径直接进 `replace_range`，不经此入口 → 规则不波及。
        if text == " " {
            // 与 `ime::replace` 同一「实际替换区间」口径（None = marked，否则选区）。
            let ins = self
                .marked
                .clone()
                .unwrap_or_else(|| self.selection_bytes());
            // 仅零宽插入且光标处于标题标记引导区 → 吞掉（不落盘、不再 notify）。
            if ins.start == ins.end && self.swallow_title_guide_space(ins.start) {
                return;
            }
        }
        crate::prof::op_start("type");
        ime::replace(self, range, text, None, false, cx);
    }

    /// S2 判定：在 `off`（源字节）处键入一个空格时是否应吞掉。条件 = 该处所在源码行
    /// 以 `{1,6} 个 '#'` 标题标记开头、且光标停在「标记引导区」——标记之后到光标为止
    /// 的前缀全为空格，且已有 ≥1 个分隔空格（第 1 个分隔空格照常插入）。一旦标记后
    /// 出现非空格字符（前缀含非空格）→ 返回 false，空格输入照常。
    pub(crate) fn swallow_title_guide_space(&self, off: usize) -> bool {
        let (line, col) = self.row_col(off);
        let lt = self.line_text(line);
        let h = lt.bytes().take_while(|&b| b == b'#').count();
        if !(1..=6).contains(&h) || col <= h {
            return false;
        }
        // CRLF：`row_col` 的列含 `\r`，而 `line_str` 不含 → 钳到行文本长，防越界切片
        // （钳到长时 `\r` 非空格 → 判定 false，安全地不吞）。
        let col = col.min(lt.len());
        lt[h..col].bytes().all(|b| b == b' ')
    }

    /// IME 组合更新（可测）：`range`（UTF-16 文档区间）为 `None` 时替换当前 marked
    /// （没有 marked 时才落普通选区/光标）——否则 Windows 每次组合更新都不带区间，
    /// 新旧组合文本会一前一后堆叠出「nnini'hni'hao你好」式垃圾串。
    /// 状态机已收敛到共享 `ime::replace`（mark=true：Atomic 意图落编辑 +
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
        ime::replace(self, range, new_text, new_selected_range, true, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    /// RED 复现宿主（镜像 `block_height_snapshot.rs` 惯例）：封装 `WyEditor` 为根元素。
    struct ReproHost {
        ed: gpui::Entity<WyEditor>,
    }
    impl gpui::Render for ReproHost {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.ed.clone()
        }
    }

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

    /// RED（复现 agent，2026-11-16）：在途 async 整篇装配期间同步 `set_source_text`，
    /// `is_loading` 卡 `true`。
    ///
    /// 机理：`set_source_text` → `install_open` 同步就地装好内容，但**不调用
    /// `invalidate_install`**（不推进 seq / 不清 loading）→ 本已同步就位的内容仍被
    /// 标记为「后台整篇装配在途」。宿主 `is_loading` 门控（替代原宿主
    /// `md_wy_install.pending`）据此拒绝增量桥/重复重喂，形成卡在途。
    ///
    /// 期望（修复后）：同步换源即装机 → `invalidate_install` 清在途 → `is_loading()==false`。
    #[gpui::test]
    fn sync_install_inflight_async_clears_loading(cx: &mut gpui::TestAppContext) {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ReproHost { ed }
        });
        let vcx: &mut gpui::VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut gpui::App| {
            let ed = host.read(cx).ed.clone();
            // 宿主 async 装配入口：取新代号 + 置「在途」。
            ed.update(cx, |w, _| {
                let s = w.next_install_seq();
                assert!(w.begin_install(s), "发起代号必为最新");
            });
            assert!(ed.read(cx).is_loading(), "发起后在途");
            // 在途期间同步换源（小文档/测试/自愈路径的同步入口）。
            ed.update(cx, |w, _| w.set_source_text("# 新文档\n新正文"));
            // RED：同步换源已就地装好内容，不应再「后台整篇装配在途」。
            assert!(
                !ed.read(cx).is_loading(),
                "RED: 同步换源后 is_loading 应 false，实际卡 true（set_source_text 未 invalidate）"
            );
        });
    }

    /// RED（复现 agent，2026-11-16）：在途 async 整篇装配期间同步 `set_source_text`，
    /// 旧 async 完成回调把内容**覆盖回旧文档**。
    ///
    /// 机理：`set_source_text`/`install_open` 不推进 seq（不留 invalidate 痕迹）→ 旧代号
    /// 仍是「最新」→ 旧完成回调的 `begin_install(old_seq)` 判当前、宿主随后
    /// `install_open(旧结果)` 把新文覆盖回旧文档（对应宿主 `md_wy_spawn_async_install`
    /// 完成路径的 `begin_install → install_open → finish_install` 序列）。
    ///
    /// 期望（修复后）：同步换源推进 seq → 旧回调 `begin_install(old_seq)` 返回 false 被丢弃。
    #[gpui::test]
    fn sync_install_inflight_async_stale_callback_does_not_overwrite(
        cx: &mut gpui::TestAppContext,
    ) {
        let (host, vcx) = cx.add_window_view(|window, cx| {
            let ed = cx.new(|cx| WyEditor::new(window, cx));
            ReproHost { ed }
        });
        let vcx: &mut gpui::VisualTestContext = vcx;
        vcx.update(|_window, cx: &mut gpui::App| {
            let ed = host.read(cx).ed.clone();
            const NEW: &str = "# 新文档\n新正文";
            let seq = ed.update(cx, |w, _| {
                let s = w.next_install_seq();
                assert!(w.begin_install(s), "发起代号必为最新");
                s
            });
            assert!(ed.read(cx).is_loading(), "发起后在途");
            // 在途期间同步换源。
            ed.update(cx, |w, _| w.set_source_text(NEW));
            assert_eq!(ed.read(cx).text().to_string(), NEW, "同步换源后立即是本文");
            // 旧 async 完成回调（宿主完成路径）：先判陈旧世序。
            let current = ed.update(cx, |w, _| w.begin_install(seq));
            if current {
                // 当前实现：同步换源未 invalidate → 旧代号仍「最新」→ 旧结果被装上新文。
                let stale =
                    WyEditor::open_from_str(gpui::SharedString::from("# 旧文档\n旧正文"));
                ed.update(cx, |w, _| {
                    w.install_open(stale);
                    w.finish_install(seq);
                });
            }
            // 期望：同步换源后旧 async 完成绝不覆盖新文。
            assert_eq!(
                ed.read(cx).text().to_string(),
                NEW,
                "RED: 旧 async 完成覆盖新文（旧回调把内容装回旧文档）"
            );
        });
    }
}
