use super::*;

use gpui::{AppContext, Pixels};

/// WyEditor 图片缓存 / 图片编辑卡片（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /// 图片加载缓存实体（元素在 paint 里 `load` 用）。
    pub(crate) fn image_cache(&self) -> gpui::Entity<gpui::RetainAllImageCache> {
        self.image_cache.clone()
    }

    /// `src` 的当前显示尺寸：`None` = 未尝试加载（发起加载/占位）；
    /// `Some(None)` = 加载失败；`Some(Some((w, h)))` = 已加载的真实宽高比尺寸。
    pub(crate) fn image_size(&self, src: &str) -> Option<Option<(f32, f32)>> {
        self.image_sizes.borrow().get(src).copied()
    }

    /// 记录 `src` 的显示尺寸；尺寸变化（占位→真实）自增 `images_gen` 使帧缓存失效，
    /// 并把「含该图的块高」收录进 `pending_image_srcs`——下一帧 `build_frame` 在**基表
    /// 复用**路径稀疏覆写受影响块高（M3 A4：图片高不再进 `height_epoch`，免占位→真实
    /// 全量 Pass1 重建，见 [`Self::patch_pending_image_heights`]）。
    pub(crate) fn set_image_size(&self, src: SharedString, size: Option<(f32, f32)>) {
        let changed = self.image_sizes.borrow().get(&src).copied() != Some(size);
        if changed {
            // 尺寸代换代：定向作废含该图的整形行（目标行缓存命中判据 = `block_content_id
            // ^ (width_bits ^ zgen)`，不含 `images_gen`——不主动作废，重建帧就会命中旧尺寸
            // 的整形行 → 图片按旧占位高/旧 `img.w` 画。见 `invalidate_image_row_cache`）。
            self.invalidate_image_row_cache(&src);
            self.image_sizes.borrow_mut().insert(src.clone(), size);
            self.images_gen.set(self.images_gen.get().wrapping_add(1));
            self.pending_image_srcs.borrow_mut().insert(src);
        }
    }

    pub(crate) fn images_gen(&self) -> u64 {
        self.images_gen.get()
    }

    /// 用户拖拽调整的图片宽（px）；未调整返回 `None`（走自然宽）。
    pub(crate) fn image_width(&self, src: &str) -> Option<f32> {
        self.image_widths.borrow().get(src).copied()
    }

    /// 记录用户拖拽调整的图片宽（px，已钳制）；变化 → `images_gen` 自增使帧/整形缓存
    /// 失效（下一帧图片按新宽 + 原宽高比重排）+ 收录进 `pending_image_srcs`（下一帧
    /// `build_frame` 在基表复用路径做为图片行高的**稀疏定向覆写**来源，免拖拽每 tick
    /// 近全量 Pass1，见 [`Self::patch_pending_image_heights`]）。不改源码。
    pub(crate) fn set_image_width(&self, src: gpui::SharedString, w: f32) {
        let clamped = w
            .clamp(
                crate::config::markdown::IMAGE_MIN_W,
                crate::config::markdown::IMAGE_MAX_W,
            )
            .max(1.);
        if self.image_widths.borrow().get(&src).copied() != Some(clamped) {
            // 宽换代：定向作废含该图的整形行——**这是拖拽改宽实时跟手的直接前提**。
            // 行整形缓存命中判据（`frame::build_frame` 的 `shape_key`）不含
            // `images_gen`，且 `block_content_id` 只含源码文本（拖拽不改源码）→ 若这里
            // 不作废，`apply_image_resize` 置 `frame=None` 后的重建帧会在 `band_hit_test`
            // 命中旧宽整形行，paint 仍按旧 `img.w` 画 → 「首拖不跟手、点外/松手才跳」
            // （真实渲染重宽 921.6 reading-col 恒定，缓存字面命中旧行；测试用视口宽
            // 1920 恰与阅读栏 921.6 不同键 → 意外 miss 而漏测）。
            self.invalidate_image_row_cache(&src);
            self.image_widths.borrow_mut().insert(src.clone(), clamped);
            self.images_gen.set(self.images_gen.get().wrapping_add(1));
            self.pending_image_srcs.borrow_mut().insert(src);
        }
    }

    /// 图片宽/尺寸变化后，**定向作废**含该 `src` 图片行的行整形缓存条目。
    ///
    /// 替代「`images_gen` 进全局 shape_key」：全局键会把整带无关文本块随每幅图 / 每次
    /// 改宽全量 miss 重排（对齐 code/math 的定向作废惯例——`on_code_parse_ready` /
    /// `on_math_ready` 同式：src_line → 块 → `block_content_id` → `invalidate_ids`）。
    ///
    /// **反向索引 O(1) 命中**：`image_src_blocks`（`src → 块下标`）直接定位相关块，
    /// 替代旧实现的两段式「当前帧行扫描（O(带内行数)）→ 全块 `parse_image_line` 兜底
    /// （O(块数)，仅缺帧/图不在带内时触发）」。索引 = 全量 Image 块扫描的**缓存结果**
    /// （键 = `content_gen`，内容任一变更即陈旧重建）：内容未变 ↦ 块集与旧全量兜底
    /// 逐块一致（判定同式：`kind == Image` 且 `parse_image_line` 反解 == `src`）；
    /// 旧帧行路径只作废带内块，反向索引覆盖全篇同 src 块（带外旧缓存行本也应按新
    /// 尺寸重排，属修复而非回归）。
    fn invalidate_image_row_cache(&self, src: &str) {
        // 陈旧（`content_gen` 变化）才整表重建一次；命中后 O(1) 取块集。
        let cur_gen = self.content_gen();
        {
            let cached = self.image_src_blocks.borrow();
            if cached.as_ref().is_none_or(|(g, _)| *g != cur_gen) {
                drop(cached);
                let built = self.build_image_src_blocks();
                *self.image_src_blocks.borrow_mut() = Some(built);
            }
        }
        let cached = self.image_src_blocks.borrow();
        let Some((_, map)) = cached.as_ref() else {
            return;
        };
        let Some(block_indices) = map.get(src) else {
            return;
        };
        if block_indices.is_empty() {
            return;
        }
        let ids: Vec<u64> = block_indices
            .iter()
            .map(|&b| frame::block_content_id(&self.doc, b))
            .collect();
        self.shape_rows.borrow_mut().invalidate_ids(&ids);
    }

    /// 从当前 Doc 的块索引**全量**构建「`src → 块下标」反向表（O(块数) 一次，缓存于
    /// `image_src_blocks`，内容任一变更即重建）：只登记独立 [`RowKind::Image`] 块（每
    /// 块一行，源行经 `parse::parse_image_line` 反解出 `src`，失败记空串——与旧全量
    /// 兜底的「跳过非 Image 块、`parsed == src` 判定」逐块一致）。
    fn build_image_src_blocks(&self) -> ImageSrcBlocks {
        let mut map: std::collections::HashMap<SharedString, Vec<usize>> =
            std::collections::HashMap::new();
        for b in 0..self.doc.block_count() {
            let Some(m) = self.doc.block_meta(b) else {
                continue;
            };
            if m.kind != crate::markdown::parse::RowKind::Image {
                continue;
            }
            let src = crate::markdown::parse::parse_image_line(
                self.doc.line_str(m.src_line as usize).trim(),
            )
            .map(|(_, s)| s)
            .unwrap_or_default();
            map.entry(src.into()).or_default().push(b);
        }
        (self.content_gen(), map)
    }

    /// **图片行高稀疏定向覆写**（M3 A4）：`build_frame` 在「基础布局复用」路径把
    /// `pending_image_srcs` 收录的 src 涉及的所有**含图块**基础高，用与 pass1 同式的
    /// [`image_block_base_height`] 逐个 Fenwick 点更新（O(#该 src 块 × log C)）。
    ///
    /// 图片占位→真实尺寸 / 拖拽改宽都走这里，而**不再进 `height_epoch`**——否则每一个
    /// 图片就绪 / 每 tick 拖拽都会触发近全量 Pass1 估算重跑（2-5ms/10 万块）。与 mgen 的
    /// 分工对齐：mgen 把数学卡高走 `fold_plan` 稀疏 delta、基础布局保持估算高；这里把
    /// 图片块高走「基础布局稀疏覆写」、基础布局整体代（`height_epoch`）保持不变（同代戳
    /// 纪律：一处构建 = `height_epoch` 的 pass1，一处失效 = 本方法，二者公式同源）。
    ///
    /// 由 `build_frame` 调用，**必须在 `find_band_range`/Pass2 之前**（二分与行高都读新
    /// 基础高）。重建路径（结构性编辑/换宽/缩放 → `rebuilt=true`）由 pass1 按当前图片
    /// 状态重估，无需本方法；残留的 src 在下次复用帧覆写（与 pass1 同式，幂等）。
    pub(crate) fn patch_pending_image_heights(
        &self,
        base: &BlockLayoutRef,
        avail: f32,
        line_h: Pixels,
    ) {
        let srcs: Vec<SharedString> = self.pending_image_srcs.borrow_mut().drain().collect();
        if srcs.is_empty() {
            return;
        }
        // 陈旧（`content_gen` 变化）才整表重建反向索引一次；命中后 O(1) 取块集。
        let cur_gen = self.content_gen();
        {
            let cached = self.image_src_blocks.borrow();
            if cached.as_ref().is_none_or(|(g, _)| *g != cur_gen) {
                drop(cached);
                let built = self.build_image_src_blocks();
                *self.image_src_blocks.borrow_mut() = Some(built);
            }
        }
        let cached = self.image_src_blocks.borrow();
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
                let h = crate::markdown::wysiwyg::shape::image_block_base_height(
                    self, src, avail, line_h,
                );
                b.set(blk, h);
            }
        }
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
    /// 图片编辑卡片当前锚定的图片行；`None` = 卡片未打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn image_edit_src_line(&self) -> Option<usize> {
        self.image_edit.as_ref().map(|p| p.src_line)
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
    ) -> Option<gpui::Entity<crate::ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.link.clone())
    }
    /// 卡片的「文本」输入框实体。
    #[cfg(test)]
    pub(crate) fn image_edit_text_input(
        &self,
    ) -> Option<gpui::Entity<crate::ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.text.clone())
    }
    /// 独立图片行 `src_line` 的当前 `(alt, src)`（`parse::parse_image_line` 反解）；
    /// 非独立图片行返回 `None`。
    fn image_line_alt_src(&self, src_line: usize) -> Option<(String, String)> {
        let raw = self.line_text(src_line);
        crate::markdown::parse::parse_image_line(raw.trim())
    }

    /// 打开 / 切换到 `src_line` 图片行的编辑卡片（仅可编辑模式）。输入框以当前
    /// alt / src 预填，打开后焦点落在「链接」输入框。已在同一行 → 关闭。
    pub(crate) fn open_image_edit(
        &mut self,
        src_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable {
            return;
        }
        if self
            .image_edit
            .as_ref()
            .is_some_and(|cur| cur.src_line == src_line)
        {
            self.close_image_edit(cx);
            return;
        }
        let Some((alt, src)) = self.image_line_alt_src(src_line) else {
            return;
        };
        // 打开编辑卡片也视为选中该图片（描边 + 按钮常显，与点图本体同效果）。
        self.selected_image.set(Some(src_line));
        let link = cx.new(|cx| {
            crate::ui::input::InputState::new(window, cx)
                .placeholder("图片链接（URL / 本地相对路径）")
                .default_value(src)
        });
        let text = cx.new(|cx| {
            crate::ui::input::InputState::new(window, cx)
                .placeholder("图片文本（alt 题注）")
                .default_value(alt)
        });
        self.image_edit = Some(ImageEditPopup {
            src_line,
            link,
            text,
        });
        // 计算并缓存卡片锚点：用**当前帧 + 上次 paint 的元素原点**（同代配对）。这保证
        // 打开后的第 1 次 render（在 paint 之前）就有正确几何；此后每帧 paint 刷新。
        // ⚠️ 不再依赖 render 期现算 frame——`end_selection` 清帧后 ensure 用 800px 宽
        // 重建会让 render 读到一个宽度错误的帧，卡片横向跳一帧。
        if let Some(f) = self.frame.borrow().as_ref().cloned() {
            let origin = self.content_origin();
            self.image_edit_anchor
                .set(self.image_edit_anchor_at(origin, &f));
        }
        // 打开后立刻聚焦「链接」输入框（用户可直接键入/IME）。
        if let Some(p) = &self.image_edit {
            let fh = p.link.read(cx).focus_handle(cx);
            fh.focus(window, cx);
        }
        cx.notify();
    }

    /// 图片编辑卡片的**窗口锚点**（卡片 TopLeft，window 坐标）：内容区水平居中 +
    /// 顶对齐目标图片行的当前屏幕 y。帧行坐标是**文档局部**（`row.top` 含整篇偏移），
    /// 窗口坐标 = 元素屏幕原点（`origin`）+ 帧局部。`origin` 必须与 `f` 同代（paint 期
    /// 用 `bounds.origin` + 本帧 frame，open 期用 `last_bounds` + 保持的帧），否则换帧
    /// 就闪。缺卡片/找不到图片行 → `None`。
    pub(crate) fn image_edit_anchor_at(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
    ) -> Option<gpui::Point<Pixels>> {
        let popup = self.image_edit.as_ref()?;
        let card_w = element::IMAGE_EDIT_CARD_W;
        let content_w = (f32::from(f.width) - f32::from(f.padding) * 2.).max(1.);
        let l = f32::from(f.padding) + ((content_w - card_w) / 2.).max(0.);
        let row_top = f
            .rows
            .iter()
            .find(|r| r.src_line == popup.src_line && r.kind == crate::markdown::ir::IrKind::Image)
            .map(|r| f32::from(r.top))
            .unwrap_or_else(|| {
                let blk = f.block_index_for_src_line(popup.src_line).unwrap_or(0);
                f.block_top(blk)
            });
        Some(point(
            px(f32::from(origin.x) + l),
            px(f32::from(origin.y) + row_top),
        ))
    }

    /// 设置图片编辑卡片锚点（`element::WyEditorElement::paint` 每帧刷新；`None` 清空）。
    pub(crate) fn set_image_edit_anchor(&self, anchor: Option<gpui::Point<Pixels>>) {
        self.image_edit_anchor.set(anchor);
    }
    /// 当前缓存的卡片锚点（测试访问器）。
    #[cfg(test)]
    pub(crate) fn image_edit_anchor(&self) -> Option<gpui::Point<Pixels>> {
        self.image_edit_anchor.get()
    }

    /// 关闭图片编辑卡片（不写源码）。
    pub(crate) fn close_image_edit(&mut self, cx: &mut Context<Self>) {
        if self.image_edit.take().is_some() {
            self.image_edit_anchor.set(None);
            cx.notify();
        }
    }

    /// 点击图片本体 → 选中该图：清除文本选区/拖选，选中态独立于指针（移开后编辑
    /// 按钮仍在，画 accent 描边）。再次点击同一图保持选中（不产生文本选区）。
    pub(crate) fn select_image(&mut self, line: usize, cx: &mut Context<Self>) {
        self.selected_image.set(Some(line));
        self.sel_anchor = None;
        self.dragging = false;
        *self.frame.borrow_mut() = None;
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
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }

    /// 保存图片编辑卡片：以输入框值改写 `![alt](src)` → `![新文本](新链接)`。
    /// 写入走 `replace_range`（撤销栈统一），随后关闭卡片。
    pub(crate) fn save_image_edit(&mut self, cx: &mut Context<Self>) {
        let Some(popup) = self.image_edit.as_ref() else {
            return;
        };
        let src_line = popup.src_line;
        let link_text = popup.link.read(cx).value().to_string();
        let text_text = popup.text.read(cx).value().to_string();
        let saved = !link_text.is_empty();
        self.apply_image_line_edit(
            src_line,
            text_text,
            if saved { link_text } else { "".into() },
            cx,
        );
        self.close_image_edit(cx);
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
        use crate::ui::button::ButtonVariants as _;
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
        let anchor = match self.image_edit_anchor.get() {
            Some(a) => a,
            None => {
                let f = self.frame.borrow();
                let f = f.as_ref()?;
                let origin = self.content_origin();
                self.image_edit_anchor_at(origin, f)?
            }
        };
        let (left, top) = (f32::from(anchor.x), f32::from(anchor.y));
        let ed = cx.entity();
        let ed_save = ed.clone();
        let ed_close = ed.clone();
        let colors = crate::ui::theme::Theme::global(cx).colors;
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
                            .child(crate::ui::input::Input::new(&link_input)),
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
                            .child(crate::ui::input::Input::new(&text_input)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_row()
                    .gap_x(px(8.))
                    .justify_end()
                    .child(
                        crate::ui::button::Button::new("wy-image-edit-cancel")
                            .label("取消")
                            .ghost()
                            .h(px(26.))
                            .px(px(12.))
                            .on_click(move |_, _: &mut Window, cx: &mut gpui::App| {
                                ed.update(cx, |e, cx| e.close_image_edit(cx));
                            }),
                    )
                    .child(
                        crate::ui::button::Button::new("wy-image-edit-save")
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
        if self.frame.borrow().is_some() {
            return true;
        }
        let width = {
            let w = self
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
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        true
    }
}
