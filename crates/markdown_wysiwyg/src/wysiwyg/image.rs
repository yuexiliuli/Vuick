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
    /// 卡片的「链接」输入框实体（render / 测试读值；跨 crate 测试探针，对齐
    /// `math_edit_input`）。
    pub fn image_edit_link_input(
        &self,
    ) -> Option<gpui::Entity<ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.link.clone())
    }
    /// 卡片的「文本」输入框实体（render / 测试读值；跨 crate 测试探针）。
    pub fn image_edit_text_input(
        &self,
    ) -> Option<gpui::Entity<ui::input::InputState>> {
        self.image_edit.as_ref().map(|p| p.text.clone())
    }
    /// 独立图片行 `src_line` 的当前 `(alt, src)`（`parse::parse_image_line` 反解）；
    /// 非独立图片行返回 `None`。
    fn image_line_alt_src(&self, src_line: usize) -> Option<(String, String)> {
        let raw = self.line_text(src_line);
        let (alt, src) = crate::markdown::parse::parse_image_line(raw.trim())?;
        // alt 经 `parse_image_line` 保持源码原样（含 `\]` 转义，供 byte_map 字节精确）——
        // 卡片/预填要**字面值**（再保存不二次转义），故此处反解（P1-8）。
        Some((
            crate::markdown::parse::unescape_image_value(&alt),
            src,
        ))
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
                // alt 在 `![` 与闭合 `]` 之间；闭合 `]` 在 src 起点前 2 字节（`](`）。
                // alt 结束由 `inline_image_spans` 的**未转义**扫描决定（`\]` 是字面 `]`，
                // 不会把 `a\]b` 误切成 `a\`，P1-8）。卡片/预填要字面值 → 反解转义。
                let alt_end = srcr.start.saturating_sub(2);
                let alt = &tail[2..alt_end.max(2)];
                let src = &tail[srcr];
                Some((
                    crate::markdown::parse::unescape_image_value(alt),
                    crate::markdown::parse::unescape_image_value(src),
                ))
            }
        }
    }

    /// 打开 / 切换到目标图片的编辑卡片（仅可编辑模式；块图 / 行内图共用同一卡片表单）。
    /// 输入框以当前 alt / src 预填，打开后焦点落在「链接」输入框。已同一目标 → 关闭。
    /// `pub`（跨 crate 可调）：宿主 shell 测试以真实 app 链路端到端打开 / 聚焦卡片，
    /// 与 `open_math_edit` 对齐（同款浮层卡）。
    pub fn open_image_edit(
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

    /// 图片编辑卡片的**窗口锚点现算**（卡片 TopLeft，window 坐标）：锚在【编辑图片】
    /// 按钮窗口矩形的**左缘**，纵向**下方优先、放不下翻上**——下方 = 按钮底缘 +
    /// `IMAGE_EDIT_CARD_GAP`；下方装不下（整卡会越编辑可视区下缘）→ 翻到按钮上方
    /// （按钮顶缘 − gap − `IMAGE_EDIT_CARD_H`），不盖按钮、不越编辑可视区下缘
    /// （状态栏之上）。
    /// 按钮几何在**元素局部**（`image_box_local`/`inline_image_box_local` +
    /// `image_edit_button_local`/`inline_image_edit_button_local`），窗口坐标 = 元素屏幕
    /// 原点（`origin`）+ 按钮局部矩形——与现有渲染/命中「paint/hit 同源同一套按钮几何」
    /// 天然对齐（点哪颗按钮 → 卡片贴哪颗按钮）。`origin` 必须与 `f` 同代（paint 期用
    /// `bounds.origin` + 本帧 frame，open 期用 `last_bounds` + 保持的帧），否则换帧就闪。
    /// 可视区上下缘：有宿主滚动句柄 → 用宿主滚动容器**视口 bounds**（`ScrollHandle::bounds()`，
    /// 由 `clamp_scroll_position` 在 paint 期写入的滚动容器窗口 bounds）：安全下缘 = 编辑区
    /// 真可视底（状态栏之上）、安全上缘 = 可视区顶。无句柄（独立测试/未注入 live）→ 回退
    /// `last_bounds()`（元素窗口 bounds）：安全下缘 = 元素屏幕原点 + 元素高（短文档/末尾图时
    /// 内容底 ≈ 可视区底）、安全上缘 = 元素屏幕原点；缺 `last_bounds`（从未 paint，无从判
    /// 可视区）→ 保守回退旧「下方」行为（snap 兜底）。⚠️ 旧实现安全下缘恒用 `last_bounds`
    /// 内容底，滚动后与 paint origin 抵消成**文档内容底**（滚动期间静止）——文档中部、按钮
    /// 下方还有内容时被误判「下方放得下」→ 卡片越可视区 → snap 兜底盖状态栏（P1，评审发现）；
    /// 改从宿主视口取真可视下缘后，图中部按钮恰近视口底即正确翻上。判定用估计高
    /// `IMAGE_EDIT_CARD_H`（保守偏大 → 偏早翻、安全优先）；`anchor_layer` 的
    /// `snap_to_window_with_margin(8)` 保留作**最后兜底**。
    /// 缺卡片 → `None`；目标行带外/未物化（找不到按钮）→ 回退 `CardAnchor::at` 的
    /// 内容区水平居中 + 顶对齐块顶（旧语义，卡片不至于飞出内容列）。
    ///
    /// 锚点几何演算（块图；行内图同构只换尺寸常量）：
    /// `image_edit_button_local_with(box, IMAGE_EDIT_BTN_SIZE=24, IMAGE_EDIT_BTN_MARGIN=6)`
    /// → 按钮左上 = (box.right − 30, box.top + 6)，边长 24 → 按钮左下 = (box.right − 30,
    /// box.top + 30)。下方锚 y0 = box.top + 30 + GAP，当 y0 + CARD_H ≤ 安全下缘 → 用 y0；
    /// 否则翻上 y1 = (box.top + 6) − GAP − CARD_H，且 y1 ≥ 安全上缘（上方也不够则夹回
    /// 安全上缘，snap 兜底最后）。
    pub(crate) fn image_edit_anchor_now(
        &self,
        origin: gpui::Point<Pixels>,
        f: &element::Frame,
    ) -> Option<gpui::Point<Pixels>> {
        let popup = self.image_edit.as_ref()?;
        let src_line = popup.target.src_line();
        // 目标【编辑图片】按钮在元素局部的矩形（块图 24×24 / 行内图 16×16，钉图片右上角）。
        let btn_local = match popup.target {
            ImageEditTarget::Block { .. } => f
                .rows
                .iter()
                .find(|r| r.src_line == src_line && r.kind == crate::markdown::ir::IrKind::Image)
                .and_then(|r| element::image_box_local(f, r))
                .map(|b| element::image_edit_button_local(&b)),
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
                        .map(|b| element::inline_image_edit_button_local(&b))
                }),
        };
        // 按钮在帧内物化 → 下方优先放不下翻上；带外/未物化行 → 回退旧语义居中顶对齐。
        let Some(btn) = btn_local else {
            let row_top = f.block_top(f.block_index_for_src_line(src_line).unwrap_or(0));
            return Some(self.image_edit_anchor.at(origin, f, row_top));
        };
        let gap = px(element::IMAGE_EDIT_CARD_GAP);
        let card_h = px(element::IMAGE_EDIT_CARD_H);
        // 可视区上下缘（window 坐标）：优先**宿主滚动容器视口 bounds**（`ScrollHandle::bounds()`，
        // paint 期 `clamp_scroll_position` 写入滚动容器窗口 bounds；随滚动静止，是编辑区真可视
        // 下/上缘、状态栏之上）。viewport 有效（高 > 0）才用；句柄缺失或从未被滚动容器 paint
        // （bounds 仍默认 0×0）→ 回退 `last_bounds` 内容底（短文档/末尾图场景仍成立）；缺
        // `last_bounds`（从未 paint）→ 保守回退旧「下方」行为（snap 兜底），不猜。`RefCell` 借用
        // 立即释放（clone 后即丢），不会跨 re-entrant 持 Ref（参照借用硬化姿势）。
        let viewport = self
            .host_scroll
            .borrow()
            .clone()
            .and_then(|(vh, _)| {
                let b = vh.bounds();
                (f32::from(b.size.height) > 0.).then_some(b)
            });
        let (safe_top, safe_bottom) = match viewport {
            Some(b) => (b.top(), b.bottom()),
            None => match self.last_bounds() {
                Some(b) => (origin.y, b.origin.y + b.size.height),
                None => {
                    return Some(gpui::point(origin.x + btn.left(), origin.y + btn.bottom() + gap))
                }
            },
        };
        // 下方放得下（整卡 ≤ 安全下缘）→ 用下方；否则翻上并夹到安全上缘（不越编辑区顶，
        // snap 最后兜底）。
        let below_y = origin.y + btn.bottom() + gap;
        let y = if f32::from(below_y) + element::IMAGE_EDIT_CARD_H <= f32::from(safe_bottom) {
            below_y
        } else {
            (origin.y + btn.top() - gap - card_h).max(safe_top)
        };
        Some(gpui::point(origin.x + btn.left(), y))
    }

    /// 关闭图片编辑卡片（不写源码）。
    pub(crate) fn close_image_edit(&mut self, cx: &mut Context<Self>) {
        if self.image_edit.take().is_some() {
            self.image_edit_anchor.clear();
            cx.notify();
        }
    }

    /// 图片编辑卡「链接 / 文本」输入框**当前是否持有焦点**（app 层 `PasteNormalized`
    /// 路由判定 + WYSIWYG `paste` 路由判定：Ctrl+V 落在卡内输入框时据此把剪贴板喂给
    /// 当前聚焦的那个输入框，而非源码编辑区）。与 `math_edit_focused` 同款镜像。
    pub(crate) fn image_edit_input_focused(
        &self,
        window: &mut Window,
        cx: &gpui::App,
    ) -> bool {
        self.image_edit
            .as_ref()
            .map(|p| {
                p.link.read(cx).focus_handle(cx).is_focused(window)
                    || p.text.read(cx).focus_handle(cx).is_focused(window)
            })
            .unwrap_or(false)
    }

    /// 图片编辑卡**任一输入框聚焦**时把 `text` 粘贴进**当前聚焦的那一个**输入框
    /// （链接框/文本框）的**光标/选区处**——用 `EntityInputHandler::replace_text_in_range
    /// (state, None, …)`（`None` 区间 = 替换当前 marked/选区 → 光标处插入，即粘贴插入
    /// 语义；调用方已做 `\r\n` 归一化，勿重复）。不用 `InputState::replace`（那是整体
    /// 替换，不是粘贴插入语义）。返回 `true` = 本次粘贴已路由到卡片，调用方（WYSIWYG
    /// 的 `paste` / app 的 `paste_normalized`）应直接返回，不再落到文档/主编辑区。
    pub fn paste_into_focused_image_edit(
        &self,
        window: &mut Window,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.image_edit_input_focused(window, cx) {
            return false;
        }
        let Some(p) = &self.image_edit else {
            return false;
        };
        use gpui::EntityInputHandler as _;
        // 链接框聚焦 → 粘链接框；否则（文本框聚焦）→ 粘文本框。
        if p.link.read(cx).focus_handle(cx).is_focused(window) {
            p.link
                .update(cx, |s, cx| s.replace_text_in_range(None, text, window, cx));
        } else {
            p.text
                .update(cx, |s, cx| s.replace_text_in_range(None, text, window, cx));
        }
        true
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
        // old_src 反解字面值（`\(`/`\)`/`\]` → 字面）——空链接保留原 src 时若原样带
        // 转义再保存会二次转义（P1-8）。
        Some((
            ls + span.start..ls + span.end,
            crate::markdown::parse::unescape_image_value(&tail[srcr]),
        ))
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
        use gpui::{InteractiveElement, ParentElement, Styled};
        let popup = self.image_edit.as_ref()?;
        // 卡片几何（**窗口坐标**，供 `anchored(Window)` 定位）。**直接读 paint 期缓存好的
        // 锚点**（`image_edit_anchor`：同代 origin+frame 算出的窗口 TopLeft）——render 期
        // 现算 frame 会读到 `end_selection` 清帧后 `ensure_frame_for_input` 兜底 800 宽
        // 重建的临时帧（与真实宽不同），卡片横向跳一帧（往左闪烁）。锚点未就绪才回退到
        // 逐帧现算；再没有帧则本帧不画卡片（绝不画角落）。
        let anchor = self.image_edit_anchor.resolve(|| {
            // ⚠️ 回退闭包内**先把帧 Rc 克隆取出、释放 `self.rd`/帧借用，再算 origin**——
            // 勿让 `self.rd.borrow()` 的 `Ref` 存活到 `content_origin()`（内部
            // `reading_col_shift` 再 `self.rd.borrow()` 同帧重借；共享×共享当前不 panic，
            // 但持 Ref 跨 re-entrant 调用是借用事故温床，同类教训见
            // `prewarm_band_double_borrow`——那处是共享×互斥直接 `RefCell already borrowed`）。
            let f = {
                let rd = self.rd.borrow();
                rd.frame.borrow().as_ref().cloned()?
            };
            let origin = self.content_origin();
            self.image_edit_anchor_now(origin, &f)
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
            // Enter 动作层拦截：deferred 浮层经 `set_active_node(prepaint_deferred_draws,
            // vendor window.rs:3931)` 挂回 WyEditor dispatch 路径，而 gpui 按键**先动作层
            // 后 key_down**（`dispatch_key_event` 先 `for binding in match_result.bindings {
            // dispatch_action_on_node }`、后 `finish_dispatch_key_event` 才派发 key_down）→
            // 卡片输入框在 key_down 层的 `stop_propagation` 拦不住 WyEditor 根的
            // `on_action(Enter→newline)`（mod.rs:615 + bind_keys mod.rs:519）——焦点在卡片
            // 输入框按 Enter 会把换行插进 caret 处正文（2026-11-13 实测回归）。此处把
            // `Enter` 动作在**卡片根**（比 WyEditor 根更靠近焦点）消费并 `stop_propagation`，
            // WyEditor 根不再收到该动作；Esc 未绑 wy-editor 键位、走 key_down 层由输入框
            // Cancel 消费，无需动作层拦截。
            .on_action(|_: &Enter, _: &mut Window, cx: &mut gpui::App| {
                cx.stop_propagation();
            })
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
        Some(ui::float_card::anchor_layer(gpui::point(px(left), px(top)), card))
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
