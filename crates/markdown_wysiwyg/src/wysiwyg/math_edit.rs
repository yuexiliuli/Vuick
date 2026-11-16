use super::*;

use gpui::{AppContext, Pixels};

/// 块级数学公式（`$$…$$`）的双击编辑卡片。结构对齐 `image.rs` 的图片编辑卡片
/// （同一套 `deferred(anchored(Window))` 浮层 + paint 期锚点缓存模式；区别：单输入框、
/// 每键即时写回、无保存/取消按钮——写回实时生效，点卡片外即关闭）。
impl WyEditor {
    /// 数学编辑卡片当前锚定的块首源码行；`None` = 卡片未打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn math_edit_src_line(&self) -> Option<usize> {
        self.math_edit.as_ref().map(|p| p.src_line)
    }
    /// 卡片是否打开。（测试访问器）
    #[cfg(test)]
    pub(crate) fn math_edit_open(&self) -> bool {
        self.math_edit.is_some()
    }
    /// 卡片的「公式正文」多行输入区域实体（render / 测试读值；跨 crate 测试探针）。
    pub fn math_edit_input(
        &self,
    ) -> Option<gpui::Entity<editor::CodeEditor>> {
        self.math_edit.as_ref().map(|p| p.text.clone())
    }
    /// 当前拖拽把手是否进行中（测试访问器）。
    #[cfg(test)]
    pub(crate) fn math_edit_dragging(&self) -> bool {
        self.math_edit_drag.get().is_some()
    }
    /// 当前累计拖拽偏移（测试访问器）。
    #[cfg(test)]
    pub(crate) fn math_edit_delta(&self) -> gpui::Point<Pixels> {
        self.math_edit_delta.get()
    }

    /// 双击块级数学卡 → 打开数学编辑卡（预填公式正文、无定界符；光标不进块、不 reveal）。
    /// 已在该块 → 关闭（与图片编辑卡同款切换语义）。
    /// **只读模式**（预览/EPUB）：双击 = **不做任何事**——预览是 WYSIWYG 只读形态，
    /// 公式始终保持渲染后的卡片；写回依赖 `replace_range` 的 editable 门控、只读无写回
    /// 权限（弹编辑卡也无意义），也不再降级为旧「双击 reveal 源码」预览（那会把渲染
    /// 卡回退成原始 LaTeX，与只读预览恒为渲染形态冲突）。
    pub fn open_math_edit(
        &mut self,
        src_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable {
            // 只读（预览/EPUB）：双击块级公式卡 = **不做任何事**——不弹编辑卡、不落
            // 光标、不回退字符，公式保持渲染后的卡片形态（预览 = WYSIWYG 只读）。
            return;
        }
        if self
            .math_edit
            .as_ref()
            .is_some_and(|cur| cur.src_line == src_line)
        {
            self.close_math_edit(cx);
            return;
        }
        // 卡行须真的落在某 `$$…$$` 块区间内（行号可漂移，先定位再预填正文）。
        let derived = self.derived();
        let d = derived.borrow();
        let Some(idx) =
            super::view::region_containing(&d.math_regions, |r| r.range.clone(), src_line, true)
        else {
            return;
        };
        let body = d.math_regions[idx].source.clone();
        drop(d);
        // 多行输入区域 = 内嵌本应用自己的 `CodeEditor`（自带纵横滚动 → 长公式/多行
        // 天然不溢出；无语法高亮、字号对齐编辑器等宽缺省）。
        let font_override = ui::theme::Theme::global(cx)
            .typography
            .editor_default_size;
        let text = cx.new(|cx| {
            let mut ed = editor::CodeEditor::new(cx);
            ed.set_language(None);
            ed.set_text(&body);
            // 横向**尾部 gap**：长公式横向滚到底后，最后一个字符与编辑器右裁切缘之间
            // 留出这段可见空隙（`MATH_EDIT_TAIL_GAP_PX`），不再贴缘被 `overflow_hidden`
            // 裁掉/难以点中选中（变相规避）。只加成 `content_w`（缓冲文本不受影响 →
            // 写回零歧义）；默认 0，仅卡片内嵌编辑器启用（主编辑器行为不变）。
            ed.set_hscroll_tail_gap(element::MATH_EDIT_TAIL_GAP_PX);
            ed.set_font_override(Some(font_override));
            // 首帧就给内嵌编辑器一个**确定的可视宽**（= 卡片内编辑器区内容宽，见
            // `MATH_EDIT_EDITOR_W`）：否则 `sync_render_state` 首帧读不到 `element_bounds`
            // （浮层 deferred prepaint 在 render 之后才回写）会回退到**窗口宽** → 长公式的
            // `content_w_px`/`visible_chars`/横向量程按窗口宽算，尾部字符被 `overflow_hidden`
            // 裁掉、看不到也无法点中/选中。prepaint 每帧会用真实布局包围盒覆写此值。
            ed.set_element_bounds(gpui::Bounds::new(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(
                    gpui::px(element::MATH_EDIT_EDITOR_W),
                    gpui::px(element::MATH_EDIT_EDITOR_H),
                ),
            ));
            ed
        });
        self.math_edit = Some(MathEditPopup { src_line, text });
        // 上次拖拽偏移 / 拖拽把手清空（open 起锚点回到文档几何，未拖拽过）。
        self.math_edit_delta.set(point(px(0.), px(0.)));
        self.math_edit_drag.set(None);
        // 订阅输入区域 `Edited`：每键即时写回文档（`apply_math_block_edit` 内部与当前
        // region source 比对、不同才写 → 幂等防回环；写回再经帧重建自动重渲染）。
        // `math_edit_sub` 随打开重建、随关闭/换源/切只读 drop（旧实体订阅一并失效）。
        let input = self
            .math_edit
            .as_ref()
            .map(|p| p.text.clone())
            .expect("刚写入");
        self.math_edit_sub = Some(cx.subscribe(
            &input,
            move |this: &mut WyEditor,
                  _: gpui::Entity<editor::CodeEditor>,
                  _: &editor::EditorEvent,
                  cx: &mut Context<WyEditor>| {
                let value = this
                    .math_edit
                    .as_ref()
                    .map(|p| p.text.read(cx).text_string());
                if let Some(v) = value {
                    this.apply_math_block_edit(src_line, &v, cx);
                }
            },
        ));
        // 打开瞬间按当前帧算一次锚点（同 `open_image_edit`：打开后第 1 次 render 就有
        // 正确几何；此后每帧 paint 刷新）。
        if let Some(f) = self.rd.borrow().frame.borrow().as_ref().cloned() {
            let origin = self.content_origin();
            self.math_edit_anchor
                .set(self.math_edit_anchor_now(origin, &f));
        }
        // 聚焦多行输入区域（用户可直接键入/IME）。
        if let Some(p) = &self.math_edit {
            let fh = p.text.read(cx).focus_handle();
            fh.focus(window, cx);
        }
        cx.notify();
    }

    /// 关闭数学编辑卡片（不写源码——写回已实时完成，无取消语义）。
    pub(crate) fn close_math_edit(&mut self, cx: &mut Context<Self>) {
        if self.math_edit.take().is_some() {
            self.math_edit_anchor.clear();
            self.math_edit_sub = None;
            self.math_edit_drag.set(None);
            self.math_edit_delta.set(point(px(0.), px(0.)));
            cx.notify();
        }
    }

    /// 以 `new_source` 改写第 `src_line` 块级数学块的正文（保留定界符形态）。
    /// 输入区域每键 `Edited` 订阅驱动；与当前 region `source` 相同 → 跳过（幂等防回环）。
    /// 写回走 `apply_edit`（撤销栈统一入口；含 `\n` 的整段替换意图 = Atomic、每键一条
    /// 撤销单元、永不合并）。写回后把光标恢复到写前位置——否则 `replace_range` 把光标
    /// 拖进块内 → 重建帧 reveal 回源码字符串（编辑卡打开期间块必须以卡片折叠形态渲染）。
    /// 多行块：整个「正文行」区间按编辑器文本**原样**替换（允许含 `\n`，行数随编辑
    /// 变化，`replace_range` / `shift_derived_after_edit` 已处理结构平移）。
    /// 单行 `$$body$$`：只替换 `$` 内层并把 `\n` 折为空格（保持单行形态合法）。
    /// 空块 `$$\n$$`：在闭围栏行首零宽插入正文。
    pub(crate) fn apply_math_block_edit(
        &mut self,
        src_line: usize,
        new_source: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.editable {
            return;
        }
        let derived = self.derived();
        let d = derived.borrow();
        let Some(idx) =
            super::view::region_containing(&d.math_regions, |r| r.range.clone(), src_line, true)
        else {
            return;
        };
        let rng = d.math_regions[idx].range.clone();
        let cur = d.math_regions[idx].source.clone();
        drop(d);
        if new_source == cur {
            return;
        }
        let saved_caret = self.caret;
        let (start, end, text) = if rng.end == rng.start + 1 {
            // 单行 `$$body$$`：只替换两个 `$` 之间的内层（定界符形态保留；行数恒 1，
            // `\n` 折为空格保持单行合法）。
            let ls = self.line_start(rng.start);
            let le = self.line_end(rng.start).max(ls + 2);
            (
                ls + 2,
                le.saturating_sub(2).max(ls + 2),
                new_source.replace('\n', " "),
            )
        } else if rng.end == rng.start + 2 {
            // 空块 `$$\n$$` 无正文行：在闭围栏行首零宽插入正文 + 换行，让正文独占一行、
            // 闭围栏仍独立成行（只插正文会生成 `<body>$$`，`$$` 不再是整行围栏 → 派生重扫
            // 会把闭围栏及以下整篇吞进块内，见空块回归测试）。行数随编辑变化。
            let at = self.line_start(rng.start + 1);
            (at, at, format!("{new_source}\n"))
        } else {
            // 多行 `$$\n…\n$$`：只替换「正文行」区间整体（开/闭围栏行不动）；编辑器
            // 文本**原样**写回（含 `\n`，行数随编辑变化，结构平移由增量管线处理）。
            let first = self.line_start(rng.start + 1);
            // 最后一条正文行 = 闭围栏行（rng.end-1）的前一行。
            let last_line = rng.end.saturating_sub(2);
            let last_end = self.line_end(last_line);
            (first, last_end, new_source.to_string())
        };
        self.apply_edit(start..end, &text, cx);
        // 恢复写前光标（不进块、不 reveal）。写回（尤其多行增行/单行折行）会改变块内
        // 字节与行数 → 恢复的**原始字节偏移**可能落进块围栏行内 → 重建帧 reveal 回源码
        // 字符串。探测写回后光标所在源行是否落进该块区间：是则把光标推到**闭围栏之后**
        //（块外首行行首），否则保留原字节值（与旧语义完全一致，旧测试锁 `caret()==tail`）。
        let snapped = {
            let drc = self.derived();
            let d = drc.borrow();
            match super::view::region_containing(&d.math_regions, |r| r.range.clone(), src_line, true)
            {
                Some(idx) => {
                    let rng = d.math_regions[idx].range.clone();
                    let row = self.row_col(saved_caret.min(self.len())).0;
                    if rng.start <= row && row < rng.end {
                        self.line_start(rng.end)
                    } else {
                        saved_caret
                    }
                }
                None => saved_caret,
            }
        };
        self.caret = snapped;
        self.sel_anchor = None;
    }

    /// 标题栏 **拖拽把手**按下：记录抓取光标 + 抓取时缓存锚点 + 抓取前累计偏移。
    /// 卡片随窗口级 `on_window_mouse_move` 的位移 1:1 跟随（Windows 元素级 mouse_move
    /// 首帧不可靠，必须走窗口级）；`on_mouse_up` / 窗口级 mouse up 结束。
    pub(crate) fn begin_math_edit_drag(&mut self, grab: gpui::Point<Pixels>) {
        if self.math_edit.is_none() {
            return;
        }
        let base_anchor = self
            .math_edit_anchor
            .get()
            .unwrap_or_else(|| point(px(0.), px(0.)));
        self.math_edit_drag.set(Some(MathEditDrag {
            grab,
            base_anchor,
            pre_delta: self.math_edit_delta.get(),
        }));
    }

    /// 拖拽中应用到新指针位置：`位移 = pos − grab`，累计偏移 = 抓取前偏移 + 位移，
    /// 缓存锚点 = 抓取时基准锚点 + 位移（卡片跟手；paint 期刷新锚点经
    /// `math_edit_anchor_now` 叠加累计偏移，不再把拖拽位移冲掉）。
    pub(crate) fn math_edit_drag_move(&mut self, pos: gpui::Point<Pixels>) {
        let Some(d) = self.math_edit_drag.get() else {
            return;
        };
        let disp = point(pos.x - d.grab.x, pos.y - d.grab.y);
        self.math_edit_delta
            .set(point(d.pre_delta.x + disp.x, d.pre_delta.y + disp.y));
        self.math_edit_anchor
            .set(Some(point(d.base_anchor.x + disp.x, d.base_anchor.y + disp.y)));
    }

    /// 结束拖拽把手（保留累计偏移 → 卡片停在拖拽终点；下次 open 重置）。
    pub(crate) fn end_math_edit_drag(&mut self) {
        self.math_edit_drag.set(None);
    }

    /// 数学编辑卡片的**窗口锚点现算**（卡片 TopLeft，window 坐标）：复用
    /// [`CardAnchor::at`] 的内容区水平居中 + 顶对齐换算，独有部分只剩目标行查找与
    /// **拖拽累计偏移** `math_edit_delta` 叠加（未拖拽 = 0，行为与旧一致）。帧行坐标是
    /// **文档局部**（`row.top` 含整篇偏移），窗口坐标 = 元素屏幕原点（`origin`）+ 帧
    /// 局部。`origin` 必须与 `f` 同代（paint 期用 `bounds.origin` + 本帧 frame，open 期
    /// 用 `content_origin` + 保持的帧），否则换帧就闪。缺卡片/找不到目标卡行 → `None`。
    pub(crate) fn math_edit_anchor_now(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
    ) -> Option<gpui::Point<Pixels>> {
        let popup = self.math_edit.as_ref()?;
        let row_top = f
            .rows
            .iter()
            .find(|r| {
                r.src_line == popup.src_line
                    && matches!(&r.extra.card, Some(element::BlockCard::Math { .. }))
            })
            .map(|r| f32::from(r.top))
            .unwrap_or_else(|| {
                let blk = f.block_index_for_src_line(popup.src_line).unwrap_or(0);
                f.block_top(blk)
            });
        let base = self.math_edit_anchor.at(origin, f, row_top);
        let d = self.math_edit_delta.get();
        Some(point(base.x + d.x, base.y + d.y))
    }

    /// 数学编辑卡内嵌编辑器**当前是否持有焦点**（app 层 `PasteNormalized` 路由判定：
    /// Ctrl+V 落到 app 根时据此决定把剪贴板喂给卡片输入区而非源码编辑区）。
    pub(crate) fn math_edit_focused(&self, window: &mut Window, cx: &gpui::App) -> bool {
        self.math_edit
            .as_ref()
            .map(|p| p.text.read(cx).focus_handle().is_focused(window))
            .unwrap_or(false)
    }

    /// 数学编辑卡内嵌编辑器聚焦时把 `text` 粘贴进卡片输入区（换行归一化由 CodeEditor
    /// `paste_text` 承担）。返回 `true` = 本次粘贴已路由到卡片，调用方（WYSIWYG 的
    /// `paste` / app 的 `paste_normalized`）应直接返回，不再落到文档/主编辑区。
    pub fn paste_into_focused_math_edit(
        &self,
        window: &mut Window,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.math_edit_focused(window, cx) {
            return false;
        }
        let Some(p) = &self.math_edit else {
            return false;
        };
        p.text.update(cx, |ed, cx| ed.paste_text(text, cx));
        true
    }

    /// 数学编辑卡在**窗口坐标**中的包围盒（锚点缓存 + 固定尺寸估计）。用于 `on_mouse_down`
    /// 的「点卡内不关/不动文档光标」判定——对点击**落在卡浮层内但非数学卡命中区**
    /// （标题/输入框下缘等）也放行，防止点到输入框时误关卡片或误移文档光标。
    pub(crate) fn math_edit_card_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
        let a = self.math_edit_anchor.get()?;
        Some(gpui::Bounds::new(
            a,
            gpui::size(px(element::MATH_EDIT_CARD_W), px(element::MATH_EDIT_CARD_H)),
        ))
    }

    /// 数学编辑卡片的悬浮浮层（打开时返回 `Some`）：`deferred(anchored(Window))` 锚定在
    /// 目标公式卡顶部、内容区水平居中。浮层逃逸滚动容器裁剪/命中，随内容滚动跟手（同
    /// `image_edit_card_element` / `ui::menu` Dropdown 浮层约定）。写回实时生效 → 无保存
    /// 按钮；点**卡片外**任意处关闭（`on_mouse_down_out`，与图片编辑卡同款）。
    /// 卡片 = 标题栏（**兼作拖拽把手**）+ 多行编辑器区（内嵌 `CodeEditor`，固定高
    /// `MATH_EDIT_EDITOR_H`、自带纵横滚动 → 长公式不溢出）。
    pub(crate) fn math_edit_card_element(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui::{
            InteractiveElement, MouseButton, ParentElement, Styled,
        };
        let popup = self.math_edit.as_ref()?;
        // 卡片几何（**窗口坐标**，供 `anchored(Window)` 定位）：**直接读 paint 期缓存好的
        // 锚点**（同 `image_edit_card_element`——render 期现算 frame 会读到 ensure 兜底
        // 800 宽重建的临时帧，卡片横跳一帧）。锚点未就绪才回退到逐帧现算。
        let anchor = self.math_edit_anchor.resolve(|| {
            // ⚠️ 回退闭包内**先把帧 Rc 克隆取出、释放 `self.rd`/帧借用，再算 origin**——
            // 与 image.rs 锚点回退闭包同款硬化：勿让 `self.rd.borrow()` 的 `Ref` 存活到
            // `content_origin()`（内部 `reading_col_shift` 再 `self.rd.borrow()` 同帧重借；
            // 共享×共享当前不 panic，但持 Ref 跨 re-entrant 调用是借用事故温床，同类教训
            // 见 `prewarm_band_double_borrow`——那处是共享×互斥直接 `RefCell already borrowed`）。
            let f = {
                let rd = self.rd.borrow();
                rd.frame.borrow().as_ref().cloned()?
            };
            let origin = self.content_origin();
            self.math_edit_anchor_now(origin, &f)
        })?;
        let (left, top) = (f32::from(anchor.x), f32::from(anchor.y));
        let ed_close = cx.entity();
        let colors = ui::theme::Theme::global(cx).colors;
        let text_input = popup.text.clone();
        // 标题栏拖拽把手：按下记录（抓取光标 + 基准锚点）并 **stop_propagation**
        // （不关卡、不动文档光标、不拖选）；移动走窗口级 `on_window_mouse_move`
        // （Windows 上元素级 mouse_move 首帧不可靠，必须走窗口级——已在本元素 paint
        // 期注册，见 `element/mod.rs`）。
        let ed_drag = cx.entity();
        let title_bar = gpui::div()
            .id("wy-math-edit-title")
            .debug_selector(|| "wy-math-edit-title".into())
            .on_mouse_down(
                MouseButton::Left,
                move |ev: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| {
                    // 按下即记录（抓取光标 + 基准锚点）；停止传播 → 下方的文档选区 /
                    // 点外关闭逻辑不触发。
                    ed_drag.update(cx, |e, _| e.begin_math_edit_drag(ev.position));
                    cx.stop_propagation();
                },
            )
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(colors.fg)
            .font_weight(gpui::FontWeight::BOLD)
            .child("编辑公式");
        let card = gpui::div()
            .id("wy-math-edit-card")
            .debug_selector(|| "wy-math-edit-card".into())
            // 点击**卡片外**任意处 → 关闭卡片（控件内点击不外发，不误关）。
            .on_mouse_down_out(
                move |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| {
                    ed_close.update(cx, |e, cx| e.close_math_edit(cx));
                },
            )
            .w(px(element::MATH_EDIT_CARD_W))
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .occlude()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap_y(px(8.))
            .child(title_bar)
            // 多行输入区域：固定高 + 内嵌 CodeEditor（自带纵向滚动 + 超宽行横向滚动 →
            // 长公式不溢出卡片；`overflow_hidden` 双保险裁掉任何越界绘制）。
            .child(
                gpui::div()
                    .id("wy-math-edit-editor")
                    .debug_selector(|| "wy-math-edit-editor".into())
                    // Math 卡内层键上下文：压制外层 CONTEXT 的 enter→wysiwyg::Enter，
                    // 把 Enter 放行给 CodeEditor 的 key_down 原样换行。不得照搬
                    // image.rs:505 的 `on_action(Enter)` stop 形态（会吞 key_down、
                    // 公式无法换行）；须走 `MATH_EDIT_CONTEXT` 的 NoAction 压制。
                    .key_context(MATH_EDIT_CONTEXT)
                    .flex()
                    .h(px(element::MATH_EDIT_EDITOR_H))
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.bg)
                    .overflow_hidden()
                    .child(text_input),
            );
        Some(ui::float_card::anchor_layer(gpui::point(px(left), px(top)), card))
    }
}
