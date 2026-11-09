use super::*;

use std::ops::Range;

use gpui::{AppContext, Pixels};

/// WyEditor 图片缓存 / 图片编辑卡片（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /// 图片加载缓存实体（元素在 paint 里 `load` 用）。
    pub(crate) fn image_cache(&self) -> gpui::Entity<gpui::RetainAllImageCache> {
        self.image_cache.clone()
    }

    /// `src` 的当前显示尺寸：`None` = 未尝试加载（发起加载/占位）；
    /// `Some(None)` = 加载失败；`Some(Some((w, h)))` = 已加载的真实宽高比尺寸。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn image_size(&self, src: &str) -> Option<Option<(f32, f32)>> {
        self.rd.borrow().image_sizes.borrow().get(src).copied()
    }

    /// 记录 `src` 的显示尺寸；尺寸变化（占位→真实）自增 `images_gen` 使帧缓存失效，
    /// 并把「含该图的块高」收录进 `pending_image_srcs`——下一帧 `build_frame` 在**基表
    /// 复用**路径稀疏覆写受影响块高（算法体已随引擎迁 `frame::set_image_size`）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn set_image_size(&self, src: SharedString, size: Option<(f32, f32)>) {
        markdown_wysiwyg_engine::frame::set_image_size(&self.rd, &self.doc, src, size);
    }

    pub(crate) fn images_gen(&self) -> u64 {
        self.rd.borrow().images_gen.get()
    }

    /// 用户拖拽调整的图片宽（px）；未调整返回 `None`（走自然宽）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn image_width(&self, src: &str) -> Option<f32> {
        self.rd.borrow().image_widths.borrow().get(src).copied()
    }

    /// 记录用户拖拽调整的图片宽（px，已钳制）；变化 → `images_gen` 自增 + 收录稀疏覆写
    ///（算法体已随引擎迁 `frame::set_image_width`）。
    pub(crate) fn set_image_width(&self, src: gpui::SharedString, w: f32) {
        markdown_wysiwyg_engine::frame::set_image_width(&self.rd, &self.doc, src, w);
    }

    /// 图片宽/尺寸变化后，**定向作废**含该 `src` 图片行的行整形缓存条目（算法体已随
    /// 引擎迁 `frame::invalidate_image_row_cache`）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    fn invalidate_image_row_cache(&self, src: &str) {
        markdown_wysiwyg_engine::frame::invalidate_image_row_cache(&self.rd, &self.doc, src);
    }

    /// 从当前 Doc 的块索引**全量**构建「`src → 块下标」反向表（算法体已随引擎迁
    /// `frame::build_image_src_blocks`）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    fn build_image_src_blocks(&self) -> ImageSrcBlocks {
        markdown_wysiwyg_engine::frame::build_image_src_blocks(&self.rd, &self.doc)
    }

    /* ---------- 图片编辑（悬浮铅笔按钮 + 卡片表单） ---------- */

    /// 当前悬浮的图片行（`Some(src_line)` 时画铅笔按钮）。
    pub(crate) fn hover_image(&self) -> Option<usize> {
        self.hover_image.get()
    }
    /// 当前选中的图片行（`Some(src_line)` 时该图片画 accent 描边 + 编辑按钮常显）。
    pub(crate) fn selected_image(&self) -> Option<usize> {
        self.selected_image.get()
    }
    /// 当前悬浮的**行内**图片（`Some(InlineImageRef)` 时该行内图右上角浮现编辑按钮）。
    pub(crate) fn hover_inline_image(&self) -> Option<InlineImageRef> {
        self.hover_inline_image.get()
    }
    /// 当前选中的**行内**图片（`Some(InlineImageRef)` 时该行内图画 accent 描边 + 按钮常显）。
    pub(crate) fn selected_inline_image(&self) -> Option<InlineImageRef> {
        self.selected_inline_image.get()
    }
    /// 图片编辑卡片当前锚定的图片**所在源码行**；`None` = 卡片未打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn image_edit_src_line(&self) -> Option<usize> {
        self.image_edit.as_ref().map(|p| p.target.src_line())
    }
    /// 卡片是否打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn image_edit_open(&self) -> bool {
        self.image_edit.is_some()
    }
    /// 卡片的「链接」输入框实体（render / 测试读值）。
    #[cfg(test)]
    pub(crate) fn image_edit_link_input(
        &self,
    ) -> Option<gpui::Entity<ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.link.clone())
    }
    /// 卡片的「文本」输入框实体。
    #[cfg(test)]
    pub(crate) fn image_edit_text_input(
        &self,
    ) -> Option<gpui::Entity<ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.text.clone())
    }
    /// 独立图片行 `src_line` 的当前 `(alt, src)`（`parse::parse_image_line` 反解）；
    /// 非独立图片行返回 `None`。
    fn image_line_alt_src(&self, src_line: usize) -> Option<(String, String)> {
        let raw = self.line_text(src_line);
        crate::markdown::parse::parse_image_line(raw.trim())
    }

    /// 编辑卡目标的当前 `(alt, src)`：独立图片行反解整行 `![…](…)`；行内图解析该图
    /// `![…](…)` 段（`refd.start` 起的源区间）。解析失败 → `None`。
    fn image_target_alt_src(&self, target: ImageEditTarget) -> Option<(String, String)> {
        match target {
            ImageEditTarget::Block { src_line } => self.image_line_alt_src(src_line),
            ImageEditTarget::Inline(refd) => {
                let raw = self.line_text(refd.src_line);
                if refd.start >= raw.len() || raw.as_bytes().get(refd.start) != Some(&b'!') {
                    return None;
                }
                let tail = &raw[refd.start..];
                let (_, srcr) =
                    crate::markdown::parse::inline_style::inline_image_spans(tail)
                        .into_iter()
                        .next()?;
                // alt 在 `![` 与 `]` 之间。
                let rb = tail.as_bytes()[2..].iter().position(|&b| b == b']')?;
                let alt = tail[2..2 + rb].to_string();
                let src = tail[srcr].to_string();
                Some((alt, src))
            }
        }
    }

    /// 打开 / 切换到目标图片的编辑卡片（仅可编辑模式；块图 / 行内图共用同一卡片表单）。
    /// 输入框以当前 alt / src 预填，打开后焦点落在「链接」输入框。已同一目标 → 关闭。
    pub(crate) fn open_image_edit(
        &mut self,
        target: ImageEditTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable {
            return;
        }
        if self
            .image_edit
            .as_ref()
            .is_some_and(|cur| cur.target == target)
        {
            self.close_image_edit(cx);
            return;
        }
        let Some((alt, src)) = self.image_target_alt_src(target) else {
            return;
        };
        // 打开编辑卡片也视为选中该图片（描边 + 按钮常显，与点图本体同效果）。
        match target {
            ImageEditTarget::Block { src_line } => self.selected_image.set(Some(src_line)),
            ImageEditTarget::Inline(refd) => self.selected_inline_image.set(Some(refd)),
        }
        let link = cx.new(|cx| {
            ui::input::InputState::new(window, cx)
                .placeholder("图片链接（URL / 本地相对路径）")
                .default_value(src)
        });
        let text = cx.new(|cx| {
            ui::input::InputState::new(window, cx)
                .placeholder("图片文本（alt 题注）")
                .default_value(alt)
        });
        self.image_edit = Some(ImageEditPopup {
            target,
            link,
            text,
        });
        // 计算并缓存卡片锚点：用**当前帧 + 上次 paint 的元素原点**（同代配对）。这保证
        // 打开后的第 1 次 render（在 paint 之前）就有正确几何；此后每帧 paint 刷新。
        // ⚠️ 不再依赖 render 期现算 frame——`end_selection` 清帧后 ensure 用 800px 宽
        // 重建会让 render 读到一个宽度错误的帧，卡片横向跳一帧。
        if let Some(f) = self.rd.borrow().frame.borrow().as_ref().cloned() {
            let origin = self.content_origin();
            self.image_edit_anchor
                .set(self.image_edit_anchor_now(origin, &f));
        }
        // 打开后立刻聚焦「链接」输入框（用户可直接键入/IME）。
        if let Some(p) = &self.image_edit {
            let fh = p.link.read(cx).focus_handle(cx);
            fh.focus(window, cx);
        }
        cx.notify();
    }

    /// 图片编辑卡片的**窗口锚点现算**（卡片 TopLeft，window 坐标）：复用
    /// [`CardAnchor::at`] 的内容区水平居中 + 顶对齐换算，独有部分只剩目标行查找与兜底。
    /// 帧行坐标是**文档局部**（`row.top` 含整篇偏移），窗口坐标 = 元素屏幕原点（`origin`）
    /// + 帧局部。`origin` 必须与 `f` 同代（paint 期用 `bounds.origin` + 本帧 frame，
    /// open 期用 `last_bounds` + 保持的帧），否则换帧就闪。缺卡片/找不到图片行 → `None`。
    pub(crate) fn image_edit_anchor_now(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
    ) -> Option<gpui::Point<Pixels>> {
        let popup = self.image_edit.as_ref()?;
        let src_line = popup.target.src_line();
        // 行内目标：锚到该行内图的**实际顶部**（行内图可能在折行段落中部，用行顶会让
        // 卡片远离图片）；块目标：锚到图片行顶。都带「块顶」兜底（带外/未物化行）。
        let row_top = match popup.target {
            ImageEditTarget::Block { .. } => f
                .rows
                .iter()
                .find(|r| r.src_line == src_line && r.kind == crate::markdown::ir::IrKind::Image)
                .map(|r| f32::from(r.top))
                .unwrap_or_else(|| f.block_top(f.block_index_for_src_line(src_line).unwrap_or(0))),
            ImageEditTarget::Inline(refd) => f
                .rows
                .iter()
                // 行内图可能落在**折行续行**（同 `src_line` 多行）——必须精确到含该图的行，
                // 否则 `.find` 只取首行会把卡片锚到段顶（远离图片）。
                .find(|r| {
                    r.src_line == src_line
                        && r.extra
                            .inline_images
                            .iter()
                            .any(|ii| ii.display_off == refd.display_off)
                })
                .and_then(|r| {
                    r.extra
                        .inline_images
                        .iter()
                        .find(|ii| ii.display_off == refd.display_off)
                        .and_then(|ii| element::inline_image_box_local(f, r, ii))
                        .map(|b| f32::from(b.top()))
                })
                .unwrap_or_else(|| f.block_top(f.block_index_for_src_line(src_line).unwrap_or(0))),
        };
        Some(self.image_edit_anchor.at(origin, f, row_top))
    }

    /// 关闭图片编辑卡片（不写源码）。
    pub(crate) fn close_image_edit(&mut self, cx: &mut Context<Self>) {
        if self.image_edit.take().is_some() {
            self.image_edit_anchor.clear();
            cx.notify();
        }
    }

    /// 点击图片本体 → 选中该图：清除文本选区/拖选，选中态独立于指针（移开后编辑
    /// 按钮仍在，画 accent 描边）。再次点击同一图保持选中（不产生文本选区）。
    pub(crate) fn select_image(&mut self, line: usize, cx: &mut Context<Self>) {
        self.selected_image.set(Some(line));
        self.sel_anchor = None;
        self.dragging = false;
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        cx.notify();
    }

    /// 删除选中的整张图片行（含换行；末行无换行只删内容）。撤销栈统一
    /// （`apply_edit`）。删除后关闭编辑卡片、清除悬浮/选中态。
    pub(crate) fn delete_selected_image(&mut self, line: usize, cx: &mut Context<Self>) {
        let start = self.line_start(line);
        let end = self.line_start(line + 1);
        self.selected_image.set(None);
        self.image_edit = None;
        self.hover_image.set(None);
        if end > start {
            self.apply_edit(start..end, "", cx);
        } else {
            self.sel_anchor = None;
            *self.rd.borrow_mut().frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /// 保存图片编辑卡片：以输入框值改写 `![alt](src)` → `![新文本](新链接)`（块图整行 /
    /// 行内图只改该段）。写入走 `replace_range`（撤销栈统一），随后关闭卡片。
    pub(crate) fn save_image_edit(&mut self, cx: &mut Context<Self>) {
        let Some(popup) = self.image_edit.as_ref() else {
            return;
        };
        let target = popup.target;
        let link_text = popup.link.read(cx).value().to_string();
        let text_text = popup.text.read(cx).value().to_string();
        let saved = !link_text.is_empty();
        match target {
            ImageEditTarget::Block { src_line } => self.apply_image_line_edit(
                src_line,
                text_text.clone(),
                if saved { link_text.clone() } else { "".into() },
                cx,
            ),
            ImageEditTarget::Inline(refd) => self.apply_inline_image_edit(
                refd,
                text_text,
                if saved { link_text } else { "".into() },
                cx,
            ),
        }
        self.close_image_edit(cx);
    }

    /// 点击行内图片**本体** → 选中该图：清除文本选区/拖选，选中态独立于指针（移开后
    /// 编辑按钮仍在，画 accent 描边）。再次点击同一图保持选中（不产生文本选区）。
    /// 与 [`Self::select_image`]（块图）同语义。
    pub(crate) fn select_inline_image(&mut self, refd: InlineImageRef, cx: &mut Context<Self>) {
        self.selected_inline_image.set(Some(refd));
        self.sel_anchor = None;
        self.dragging = false;
        *self.rd.borrow_mut().frame.borrow_mut() = None;
        cx.notify();
    }

    /// 行内图 `![…](…)` 的**绝对源字节区间**（`!` 起、`)` 后一字节止）及旧 src。
    /// 由 `refd.start` 从源码反解（源码系身份，无需帧；行已变/解析失败 → `None`）。
    pub(crate) fn inline_image_source_range_of(
        &self,
        refd: InlineImageRef,
    ) -> Option<(Range<usize>, String)> {
        let raw = self.line_text(refd.src_line);
        if refd.start >= raw.len() || raw.as_bytes().get(refd.start) != Some(&b'!') {
            return None;
        }
        let tail = &raw[refd.start..];
        let (full, srcr) = crate::markdown::parse::inline_style::inline_image_spans(tail)
            .into_iter()
            .next()?;
        let span = refd.start + full.start..refd.start + full.end;
        if span.end > raw.len() {
            return None;
        }
        // `apply_edit`/`replace_range` 按**整篇源**字节操作（块图 `delete_selected_image`
        // 用 `line_start` 归位同式）——这里把行内相对偏移落到**全局**字节（`refd.start`
        // 是行内偏移；仅第 0 行两者相等，非首行会删错字节，M6 回归测试锁过）。
        let ls = self.line_start(refd.src_line);
        Some((ls + span.start..ls + span.end, tail[srcr].to_string()))
    }

    /// 删除选中的整段行内 `![…](…)`（保留行内其它文本，含换行归位；撤销栈统一）。
    /// 删除后关闭编辑卡片、清除悬浮/选中态。
    pub(crate) fn delete_selected_inline_image(
        &mut self,
        refd: InlineImageRef,
        cx: &mut Context<Self>,
    ) {
        self.selected_inline_image.set(None);
        self.image_edit = None;
        self.hover_inline_image.set(None);
        if let Some((span, _)) = self.inline_image_source_range_of(refd) {
            self.apply_edit(span, "", cx);
        } else {
            self.sel_anchor = None;
            *self.rd.borrow_mut().frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /// 程序化改写行内 `![…](…)` 的 alt 与 src（**只覆盖该图段**，不碰行内其它文本）。
    /// 空 `src` 视为保留原 src。供编辑卡片保存 / 程序化测试调用。
    pub(crate) fn apply_inline_image_edit(
        &mut self,
        refd: InlineImageRef,
        new_alt: String,
        new_src: String,
        cx: &mut Context<Self>,
    ) {
        let Some((span, old_src)) = self.inline_image_source_range_of(refd) else {
            return;
        };
        let src = if new_src.is_empty() { old_src } else { new_src };
        let new_text = format!(
            "![{}]({})",
            new_alt.replace(']', "\\]").replace('\n', " "),
            src.replace('(', "\\(")
                .replace(')', "\\)")
                .replace('\n', " ")
        );
        self.apply_edit(span, &new_text, cx);
    }

    /// 程序化改写第 `src_line` 行独立图片的 alt 与 src（`![alt](src)`）。
    /// 空 `src` 视为保留原 src。供图片编辑卡片保存 / 程序化测试调用。
    pub(crate) fn apply_image_line_edit(
        &mut self,
        src_line: usize,
        new_alt: String,
        new_src: String,
        cx: &mut Context<Self>,
    ) {
        let start = self.line_start(src_line);
        let end = self.line_end(src_line).max(start);
        let raw = self.doc.slice_str(start..end);
        let (_, src) = match crate::markdown::parse::parse_image_line(raw.trim()) {
            Some(pair) => pair,
            None => return,
        };
        let src = if new_src.is_empty() { src } else { new_src };
        let indent = raw.len() - raw.trim_start().len();
        let new_line = format!(
            "{}![{}]({})",
            &raw[..indent],
            new_alt.replace(']', "\\]").replace('\n', " "),
            src.replace('(', "\\(")
                .replace(')', "\\)")
                .replace('\n', " ")
        );
        self.apply_edit(start..end, &new_line, cx);
    }

    /// 图片编辑卡片的悬浮浮层（打开时返回 `Some`）：`deferred(anchored(Local))` 锚定在
    /// 目标图片顶部、内容区水平居中。浮层逃逸滚动容器裁剪/命中，随内容滚动跟手（对齐
    /// `ui::menu` 的 Dropdown 浮层约定）。
    pub(crate) fn image_edit_card_element(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use ui::button::ButtonVariants as _;
        use gpui::{
            Anchor, AnchoredPositionMode, Edges, InteractiveElement, IntoElement, ParentElement,
            Styled,
        };
        let popup = self.image_edit.as_ref()?;
        // 卡片几何（**窗口坐标**，供 `anchored(Window)` 定位）。**直接读 paint 期缓存好的
        // 锚点**（`image_edit_anchor`：同代 origin+frame 算出的窗口 TopLeft）——render 期
        // 现算 frame 会读到 `end_selection` 清帧后 `ensure_frame_for_input` 兜底 800 宽
        // 重建的临时帧（与真实宽不同），卡片横向跳一帧（往左闪烁）。锚点未就绪才回退到
        // 逐帧现算；再没有帧则本帧不画卡片（绝不画角落）。
        let anchor = self.image_edit_anchor.resolve(|| {
            let rd = self.rd.borrow();
            let f = rd.frame.borrow();
            let f = f.as_ref()?;
            let origin = self.content_origin();
            self.image_edit_anchor_now(origin, f)
        })?;
        let (left, top) = (f32::from(anchor.x), f32::from(anchor.y));
        let ed = cx.entity();
        let ed_save = ed.clone();
        let ed_close = ed.clone();
        let colors = ui::theme::Theme::global(cx).colors;
        let link_input = popup.link.clone();
        let text_input = popup.text.clone();
        let card = gpui::div()
            .id("wy-image-edit-card")
            .debug_selector(|| "wy-image-edit-card".into())
            // 点击**卡片外**任意处 → 关闭卡片（控件内点击不外发，不误关）。
            .on_mouse_down_out(
                move |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| {
                    ed_close.update(cx, |e, cx| e.close_image_edit(cx));
                },
            )
            .w(px(element::IMAGE_EDIT_CARD_W))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .occlude()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap_y(px(8.))
            .child(
                gpui::div()
                    .text_size(px(13.))
                    .text_color(colors.fg)
                    .font_weight(gpui::FontWeight::BOLD)
                    .child("编辑图片"),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_y(px(4.))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(colors.fg_muted)
                            .child("链接"),
                    )
                    // 输入框样式：内凹底色 + 圆角描边，标识「可输入」区域（区别于卡片
                    // 表面与纯文字）。聚焦态边框提亮在保存时由卡片统一不变，此处只供样式。
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .h(px(28.))
                            .w_full()
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .child(ui::input::Input::new(&link_input)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_y(px(4.))
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(colors.fg_muted)
                            .child("文本"),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .h(px(28.))
                            .w_full()
                            .px_2()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.bg)
                            .child(ui::input::Input::new(&text_input)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_row()
                    .gap_x(px(8.))
                    .justify_end()
                    .child(
                        ui::button::Button::new("wy-image-edit-cancel")
                            .label("取消")
                            .ghost()
                            .h(px(26.))
                            .px(px(12.))
                            .on_click(move |_, _: &mut Window, cx: &mut gpui::App| {
                                ed.update(cx, |e, cx| e.close_image_edit(cx));
                            }),
                    )
                    .child(
                        ui::button::Button::new("wy-image-edit-save")
                            .label("保存")
                            .primary()
                            .h(px(26.))
                            .px(px(12.))
                            .on_click(move |_, _: &mut Window, cx: &mut gpui::App| {
                                ed_save.update(cx, |e, cx| e.save_image_edit(cx));
                            }),
                    ),
            );
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position_mode(AnchoredPositionMode::Window)
                    .anchor(Anchor::TopLeft)
                    .position(gpui::point(px(left), px(top)))
                    .snap_to_window_with_margin(Edges::all(px(8.)))
                    .child(card),
            )
            .with_priority(100)
            .into_any_element(),
        )
    }

    /// 输入命中前确保帧存在（帧被置空后、下一次渲染还没重建时，点击才会落空——
    /// 这正是「展开后立刻再点折叠」偶发失效的来源）。缺帧则当场按当前可视带重建。
    pub(crate) fn ensure_frame_for_input(&mut self, window: &mut Window) -> bool {
        if self.rd.borrow().frame.borrow().is_some() {
            return true;
        }
        let width = {
            let w = self
                .rd
                .borrow()
                .frame
                .borrow()
                .as_ref()
                .map(|f| f.width)
                .unwrap_or(px(800.));
            if f32::from(w) <= 1. {
                return false;
            }
            w
        };
        let band = self.shape_band().unwrap_or((0., 5000.));
        let frame = element::build_frame(self, window, width, band);
        *self.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        true
    }
}
