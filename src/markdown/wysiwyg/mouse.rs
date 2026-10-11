use super::*;

use crate::markdown::parse::recognition::{self, LinkHit};

/// WyEditor 鼠标输入 / 命中 / 叙事条与图卡横向滚动条交互（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /* ---------- 点击 / 鼠标 ---------- */

    /// 鼠标按下命中 → 源偏移（经 frame + byte_map）；左键开始拖选并抢焦点。
    /// 先试交互性命中：任务框翻转（C1）、警报折叠箭头（C3）、行内链接（C5）。
    pub(crate) fn on_mouse_down(
        &mut self,
        ev: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ev.button != gpui::MouseButton::Left {
            return;
        }
        // 任何新左键按下都先清残留的图片调整条拖拽：上次松开若落在元素/顶层叠层外，
        // 元素级 `on_mouse_up`（受 `hitbox.is_hovered` 门控）未被派发 → 状态残留，
        // 之后「仅移动鼠标 / 点图片外」就会误触发 `apply_image_resize` 突然改宽。
        // 这里一进 `mouse_down` 就清干净；若本次按下命中调整条，后面的 `begin_image_resize`
        // 会重新建立（全新一次拖拽，抓取点/起始宽都不带旧状态）。
        self.end_image_resize();
        // 输入命中前保证帧存在（缺帧当场重建，避免展开后立刻再点折叠时命中落空）。
        if !self.ensure_frame_for_input(window) {
            return;
        }
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        // 任何点击都先清除旧图片选中（图片本体/编辑按钮分支随后重新置位）：
        // 点文本/任务框/链接等 → 图片不再保持选中态。
        self.selected_image.set(None);
        // **mermaid 卡点击 → 聚焦**：聚焦后滚轮才缩放该卡（交互见图文，未聚焦时滚轮
        // 一律正常滚动）。点击**任何其它地方**（含选中/链接/任务框）→ 清除聚焦。
        if let Some(mline) = element::mermaid_card_hit(self, f, ev.position) {
            drop(marker);
            self.focus.focus(window, cx);
            self.focused_mermaid = Some(mline);
            cx.notify();
            return;
        }
        self.focused_mermaid = None;
        // 交互性命中（任务框/折叠箭头/链接）单双击都响应，展开→立即再点也能折叠回。
        if !ev.modifiers.shift && !ev.modifiers.control {
            // 图片编辑铅笔按钮：仅可编辑模式——点击 → 打开/切换/关闭编辑卡片
            // （不落光标、不进入选区）。只读预览不响应（不画按钮、这里也不拦截）。
            if self.editable
                && let Some(line) = element::image_edit_button_hit_at(self, f, ev.position)
            {
                drop(marker);
                self.open_image_edit(line, window, cx);
                return;
            }
            // 图片调整条（右中边缘）：按住 → 开始拖拽改宽（保持宽高比、不改源码）。
            // 在「点图本体选中」之前判定（条在图片盒内）。
            if self.editable
                && let Some((line, grab_x)) =
                    element::image_resize_handle_hit_at(self, f, ev.position)
            {
                let (src, start_w) = f
                    .rows
                    .iter()
                    .find(|r| r.src_line == line && r.kind == crate::markdown::ir::IrKind::Image)
                    .and_then(|r| r.image.as_ref())
                    .map(|i| (i.src.clone(), i.w))
                    .unwrap_or((gpui::SharedString::default(), 0.));
                drop(marker);
                if src.is_empty() {
                    return;
                }
                self.focus.focus(window, cx);
                self.selected_image.set(Some(line));
                self.begin_image_resize(src, start_w, grab_x);
                *self.frame.borrow_mut() = None;
                cx.notify();
                return;
            }
            // 点击图片**本体**（图片盒，不含下方题注行）→ 选中该图：不落光标进题注
            //（修掉「点图 → 光标跳到图片下方描述文本」）。选中后图片画 accent 描边 +
            // 编辑按钮常显；Backspace/Delete 删除整张图片行。
            if self.editable
                && let Some(line) = element::image_box_hit_at(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                self.select_image(line, cx);
                return;
            }
            // 表格横向滚动条（内容超宽的表格底部轨道）：点在**滑块**上 → 拖动；
            // 点在**轨道**上 → 滑块直接跳到点击处（view 的 gpui-component 语义）。
            if let Some((blk, on_thumb, grab)) = element::table_hscrollbar_hit(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                if on_thumb {
                    *self.table_h_drag.borrow_mut() = Some((blk, grab));
                    self.table_h_drag_apply(ev.position, cx);
                } else {
                    self.table_bar_jump_to(blk, ev.position);
                    cx.notify();
                }
                return;
            }
            // 代码块横向滚动条（内容超宽的块底部轨道）：点在**滑块**上 → 拖动；
            // 点在**轨道**上 → 滑块直接跳到点击处（view 的 gpui-component 语义）。
            if let Some((blk, on_thumb, grab)) = element::code_hscrollbar_hit(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                if on_thumb {
                    *self.code_h_drag.borrow_mut() = Some((blk, grab));
                    self.code_h_drag_apply(ev.position, cx);
                } else {
                    // 跳转：滑块中心对准点击点（·`rel = (px − bar_x − thumb_w/2)/travel`）。
                    self.code_bar_jump_to(blk, ev.position);
                    cx.notify();
                }
                return;
            }
            // 数学卡横向滚动条（超宽公式卡的底部轨道）：滑块 → 拖；轨道 → 跳转
            // （与代码块/表格同语义；长公式不缩小、块内横滚）。
            if let Some((blk, on_thumb, grab)) = element::math_hscrollbar_hit(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                if on_thumb {
                    *self.math_h_drag.borrow_mut() = Some((blk, grab));
                    self.math_h_drag_apply(ev.position, cx);
                } else {
                    self.math_bar_jump_to(blk, ev.position);
                    cx.notify();
                }
                return;
            }
            // C1: 点击任务框 → 翻转 `[ ]` ↔ `[x]`（写源码，走撤销栈）。
            if let Some((line, _)) = element::checkbox_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_checkbox_at_line(line, cx);
                return;
            }
            // C3: 点击警报折叠箭头 → 翻转 `-`/`+`（折叠/展开 callout 主体）。
            if let Some((line, _)) = element::alert_chevron_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_alert_fold_at_line(line, cx);
                return;
            }
            // 标题折叠箭头 → 翻转视图本地折叠态（`## Goals` body 行折叠/展开）。
            if let Some((line, _)) = element::heading_chevron_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.toggle_heading_fold_at_line(line, cx);
                return;
            }
            // C5: 点击行内链接 → 打开（安全 URL 直开系统浏览器；wiki/tag → OpenPage）。
            if let Some(hit) = element::link_hit_at(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                match hit {
                    LinkHit::Url(u) if recognition::is_safe_external_url(&u) => {
                        cx.open_url(&u);
                    }
                    LinkHit::Url(u) | LinkHit::Page(u) => {
                        cx.emit(WyEvent::OpenPage(u));
                    }
                    LinkHit::BlockRef(id) => {
                        cx.emit(WyEvent::OpenPage(format!("#^{id}")));
                    }
                }
                return;
            }
            // P4-2: 点击注记标记 `[n]` → 文内引用 ↔ 末尾定义**双向跳转**（仅只读宿主，
            // 对齐 A 侧预览语义；可编辑模式保留文本编辑、不劫持点击）。跳到对端源码行并
            // 加亮对端标记，悬浮卡随之关闭（`navigate_marker` 内清 `hover_note`）。
            if !self.editable
                && let Some(hit) = element::note_marker_hit_at(self, f, ev.position)
            {
                drop(marker);
                self.focus.focus(window, cx);
                self.navigate_marker(&hit, cx);
                return;
            }
            // A1/B2: 数学交互——
            // 数学块卡片：**单击不改光标**（否则 caret 进块被 reveal、整块回显源码字符串，
            // 即用户看到的「单击变回字符串」）；**双击**进块编辑（reveal 源码可改）。
            // 行内公式：**双击** spacer → 光标放进 `$…$` 内（shape 期 reveal 可编辑）。
            let dbl = ev.click_count >= 2;
            if let Some(block_start) = element::math_card_hit(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                if dbl {
                    // 光标放进 `$$` 行**内容内 1 字节**（不是行首 0 列）：
                    // ①块 reveal ②单行 `$$…$$` 的行内 splice reveal 同时满足
                    // → 两种情况（多行块 / 单行块）双击都直接还原成源码字符串
                    // （放行首 0 列时 `splice_math_text` 的 `c > span.start` 不成立，
                    // 单行块会被 splice 成小号行内公式，需再双击一次才还原）。
                    self.set_caret(block_start + 1);
                    cx.notify();
                }
                return;
            }
            if dbl && let Some(src) = element::inline_math_hit_at(self, f, ev.position) {
                drop(marker);
                self.focus.focus(window, cx);
                self.set_caret(src.start + 1);
                cx.notify();
                return;
            }
        }
        let off = element::source_offset_at(self, f, ev.position);
        drop(marker);
        self.focus.focus(window, cx);
        self.begin_selection_at(off);
        cx.notify();
    }

    /// 拖拽中（左键已按下）。
    pub(crate) fn on_mouse_move(&mut self, ev: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        // 图片调整条拖拽优先：按住条 → 随鼠标横向改宽（不参与选区拖选/悬浮）。
        // ⚠️ 不再做 `pressed_button == Some(Left)` 门控：`image_resize_drag` 只在真实
        // 「按下命中句柄」的 `on_mouse_down` 里置位，松开/新按下由四条边界统一清位
        //（元素内 `on_mouse_up` + 元素外 `on_mouse_up_out` + 窗口级 `on_window_mouse_up`
        // + 任何新左键 `on_mouse_down` 顶部），状态自身即「本帧正在拖」的事实来源。
        // 若再依赖平台按钮标志，Windows 上**第一次**拖拽经窗口级兜底路径的 move 拿不到
        // 该标志（GPUI 派发到元素/窗口监听时标志首拍并不可靠）→ 首拖整段不 apply =
        // 「第一次拖动无效、点别处才一次性应用」（用户真机复验症状）。去掉后首拖即跟手，
        // 而「松开/点外后纯移动不改宽」仍由上述边界闭环保证（不依赖平台标志的可靠性）。
        if self.image_resize_drag.borrow().is_some() {
            self.apply_image_resize(ev.position.x, cx);
            return;
        }
        // 代码/表格/数学卡横向滚动条拖动优先（不参与选区拖选）。
        if self.code_h_drag.borrow().is_some() {
            self.code_h_drag_apply(ev.position, cx);
            return;
        }
        if self.table_h_drag.borrow().is_some() {
            self.table_h_drag_apply(ev.position, cx);
            return;
        }
        if self.math_h_drag.borrow().is_some() {
            self.math_h_drag_apply(ev.position, cx);
            return;
        }
        // 未拖选：更新图片悬浮态（命中图片盒 → 该图右上角浮现编辑按钮）。
        // 拖选中不更新（指针离开图片也不应隐藏按钮，避免拖到别处时误闪烁）。
        if !self.dragging {
            self.update_hover(ev.position, cx);
            self.update_note_hover(ev.position, cx);
            // 图片调整条 hover → 即时换 resize 光标（同 `on_window_mouse_move`）。
            if self.notify_when_hover_grip(ev.position) {
                cx.notify();
            }
            return;
        }
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let off = element::source_offset_at(self, f, ev.position);
        drop(marker);
        self.extend_selection_to(off);
        *self.frame.borrow_mut() = None;
        cx.notify();
    }
    /// 更新图片悬浮态（命中图片盒 → `hover_image`）。未命中（含指针移出元素）置 `None`。
    fn update_hover(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let hovered = element::image_box_hit_at(self, f, pos);
        drop(marker);
        if self.hover_image.get() != hovered {
            self.hover_image.set(hovered);
            cx.notify();
        }
    }

    /// 更新注记悬浮态（`hover_note`，P4-2）：命中注记标记（只读宿主）→ 记录命中
    ///（含内容列局部矩形，悬浮卡锚定用）；移出标记 / 可编辑模式 → 清除。与
    /// `hover_image` 同款「变才 notify」。
    fn update_note_hover(&mut self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        if self.editable {
            if self.hover_note.get().is_some() {
                self.hover_note.set(None);
                cx.notify();
            }
            return;
        }
        let marker = self.frame.borrow();
        let hit = marker
            .as_ref()
            .and_then(|f| element::note_marker_hit_at(self, f, pos));
        drop(marker);
        let cur = self.hover_note.get();
        let changed = match (&cur, &hit) {
            (Some(a), Some(b)) => {
                a.src_line != b.src_line || a.n != b.n || a.is_def != b.is_def
            }
            (None, None) => false,
            _ => true,
        };
        if changed {
            self.hover_note.set(hit);
            cx.notify();
        }
    }

    /// 窗口级 mouse-move 的悬浮更新（图片编辑按钮亮显）：元素在宿主 `#live-vscrollbar-overlay`
    /// 等顶层叠层之下时，元素级 `on_mouse_move` 会被遮罩吞掉（与滚轮同因），走
    /// `window.on_mouse_event`（Bubble）由元素 paint 期注册驱动。指针在本元素
    /// `last_bounds` 内才更新 `hover_image`，移出即清除。拖选中（鼠标按下）由元素级
    /// 拖选事件驱动，这里跳过。
    pub(crate) fn on_window_mouse_move(
        &mut self,
        ev: &gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        // 图片调整条拖拽优先（顶层叠层会吞元素级 move，拖动期间走这里兜底）。
        // 与元素级 `on_mouse_move` 同款语义：不依赖 `pressed_button`（首拖经此窗口级
        // 路径时该标志不可靠，见 `on_mouse_move` 注释）；「松开/点外后纯移动不改宽」
        // 由 `end_image_resize` 四条边界闭环保证。
        if self.image_resize_drag.borrow().is_some() {
            self.apply_image_resize(ev.position.x, cx);
            return;
        }
        if self.dragging {
            return;
        }
        let Some(b) = self.last_bounds() else {
            return;
        };
        let inside_image = b.contains(&ev.position);
        if inside_image {
            self.update_hover(ev.position, cx);
        } else if self.hover_image.get().is_some() {
            self.hover_image.set(None);
            cx.notify();
        }
        // 注记悬浮（只读宿主）：窗口级 move 兜底——元素在宿主顶层叠层之下时元素级
        // `on_mouse_move` 会被遮罩吞掉（与滚轮同因），这里与 `hover_image` 同法更新。
        self.update_note_hover(ev.position, cx);
        // 指针进入图片调整条（右中边缘）→ 立即重绘：paint 登记窗口级 resize 光标，
        // `finish_frame` 按新帧当场解析——否则要等下一次输入/点击光标才变
        //（「hover 不变 → 先点才显示」）。
        if self.notify_when_hover_grip(ev.position) {
            cx.notify();
        }
    }

    /// 指针落在图片调整条（当前帧，可编辑）→ `true`（调用方 `notify` 触发一次重绘，
    /// 让 resize 光标即时生效）。
    fn notify_when_hover_grip(&self, pos: gpui::Point<Pixels>) -> bool {
        if !self.editable {
            return false;
        }
        let marker = self.frame.borrow();
        marker
            .as_ref()
            .and_then(|f| element::image_resize_handle_hit_at(self, f, pos))
            .is_some()
    }
    /// 代码块滚动条**轨道点击跳转**：滑块中心对准点击点（view 语义：不在滑块上
    /// 按轨道即跳转，而不是像旧实现那样点空白处无反应）。
    fn code_bar_jump_to(&self, blk: usize, pos: gpui::Point<Pixels>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f
            .rows
            .iter()
            .find(|r| r.src_line == blk && !r.multi_offsets.is_empty())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::code_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        // 滑块中心对准点击点：center = p_local → thumb_x = p_local − thumb_w/2。
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_code_h_scroll(blk, rel * max_scroll);
    }
    /// 把指针位置应用到正在拖动的代码块横向滚动条（保持**抓取点相对滑块左缘**：
    /// `thumb_x = px - grab`，按下瞬间 scroll 不变、拖动时滑块钉住抓取点 1:1 跟随）。
    fn code_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = *self.code_h_drag.borrow() else {
            return;
        };
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f
            .rows
            .iter()
            .find(|r| r.src_line == blk && !r.multi_offsets.is_empty())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::code_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_code_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }
    /// 表格滚动条**轨道点击跳转**：滑块中心对准点击点（与 [`Self::code_bar_jump_to`] 同语义）。
    fn table_bar_jump_to(&self, blk: usize, pos: gpui::Point<Pixels>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f
            .rows
            .iter()
            .find(|r| r.src_line == blk && r.table.is_some())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::table_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
    }
    /// 把指针位置应用到正在拖动的表格横向滚动条（抓取点相对滑块左缘，1:1 跟随）。
    fn table_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = *self.table_h_drag.borrow() else {
            return;
        };
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f
            .rows
            .iter()
            .find(|r| r.src_line == blk && r.table.is_some())
        else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::table_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_table_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }
    /// 数学卡滚动条**轨道点击跳转**：滑块中心对准点击点（与 [`Self::code_bar_jump_to`] 同语义）。
    fn math_bar_jump_to(&self, blk: usize, pos: gpui::Point<Pixels>) {
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f.rows.iter().find(|r| {
            r.src_line == blk && matches!(&r.extra.card, Some(element::BlockCard::Math { .. }))
        }) else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::math_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - thumb_w / 2.0) / travel).clamp(0., 1.);
        drop(marker);
        self.set_math_h_scroll(blk, rel * max_scroll);
    }
    /// 把指针位置应用到正在拖动的数学卡横向滚动条（抓取点相对滑块左缘，1:1 跟随）。
    fn math_h_drag_apply(&self, pos: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((blk, grab)) = *self.math_h_drag.borrow() else {
            return;
        };
        let marker = self.frame.borrow();
        let Some(f) = marker.as_ref().map(|r| &**r) else {
            return;
        };
        let Some(r) = f.rows.iter().find(|r| {
            r.src_line == blk && matches!(&r.extra.card, Some(element::BlockCard::Math { .. }))
        }) else {
            return;
        };
        let Some((bar_x, bar_w, _, thumb_w, max_scroll)) = element::math_hscroll_geo(self, f, r)
        else {
            return;
        };
        let origin_x = self
            .last_bounds()
            .map(|b| f32::from(b.origin.x))
            .unwrap_or(0.);
        let elem_w = self
            .last_bounds()
            .map(|b| f32::from(b.size.width))
            .unwrap_or(0.);
        // 居中阅读栏：元素屏幕原点外再减列移位，窗口坐标才落在内容区内部坐标系。
        let p_local = f32::from(pos.x) - origin_x - self.reading_col_shift(elem_w);
        let travel = (bar_w - thumb_w).max(1.);
        let rel = ((p_local - bar_x - grab) / travel).clamp(0., 1.);
        drop(marker);
        self.set_math_h_scroll(blk, rel * max_scroll);
        cx.notify();
    }
    /// 鼠标松开：结束拖选 / 图片调整条 / 代码块 & 表格 & 数学卡横向滚动条拖动。
    pub(crate) fn on_mouse_up(&mut self, _: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        self.end_selection();
        self.end_image_resize();
        *self.code_h_drag.borrow_mut() = None;
        *self.table_h_drag.borrow_mut() = None;
        *self.math_h_drag.borrow_mut() = None;
        cx.notify();
    }

    /// **窗口级鼠标松开兜底**（元素 paint 期 `window.on_mouse_event` 注册，无 hover 门控）：
    /// 松开点落在元素 `hitbox` 外 / 顶层叠层上 / 窗口外时，元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不会被派发 → 图片调整条拖拽状态残留，后续任何移动都会
    /// 触发 `apply_image_resize` 突然改宽。这里无论指针在哪、不读帧几何，只要有 MouseUp
    /// 就无条件清掉残留的调整条拖拽（与元素级 `on_mouse_up` / `on_mouse_up_out` 重复执行
    /// 幂等）。⚠️ 只清图片调整条：拖选/选区由元素级 `on_mouse_up` 管理（窗口级这里绕过
    /// hover 门控，碰选区会与元素级重复/错序）。同时被元素级 `on_mouse_up_out`（元素外
    /// 松开，Capture 阶段）复用，形成「松开/新按下」四条清态边界：
    /// `on_mouse_down` 顶部 + 元素内 `on_mouse_up` + 元素外 `on_mouse_up_out` + 本窗口级。
    pub(crate) fn on_window_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.image_resize_drag.borrow().is_some() {
            self.end_image_resize();
            cx.notify();
        }
    }

    /// 滚轮：**仅在光标悬停在 mermaid 卡片上时**拦截（普通滚轮 = 整卡缩放），其余位置
    /// 返回 `false` 放行给宿主滚动/`zoom_overlay`。由元素 paint 期 `window.on_mouse_event`
    /// （bubble）驱动——元素在宿主 `zoom_overlay`（顶层兄弟）之下，元素级 `.on_scroll_wheel`
    /// 会被兄弟遮罩吞掉，必须走窗口级事件监听。
    ///
    /// 缩放走 `zoom_mermaid_at`：先捕捉视口锚点、重排后再写回宿主 `ScrollHandle`——页面
    /// **不同步滚动**（缩放改的是卡高，内容随锚点原地钉住）。
    pub(crate) fn on_scroll_wheel(
        &mut self,
        ev: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ev.modifiers.control {
            return false; // Ctrl 放行给宿主 zoom_overlay（全局字号缩放）。
        }
        let Some(b) = self.last_bounds() else {
            return false;
        };
        if !b.contains(&ev.position) {
            return false;
        }
        let local_y = f32::from(ev.position.y) - f32::from(b.origin.y);
        let hit_mermaid = self
            .frame
            .borrow()
            .as_ref()
            .and_then(|f| {
                f.rows.iter().find(|r| {
                    local_y >= f32::from(r.top) && local_y < f32::from(r.top) + f32::from(r.height)
                })
            })
            .filter(|r| matches!(&r.extra.card, Some(element::BlockCard::Mermaid)))
            .map(|r| r.src_line);
        let Some(block) = hit_mermaid else {
            return false;
        };
        // **仅聚焦的 mermaid 消费滚轮**（点击卡片后才可缩放）：未聚焦 → 放行宿主正常
        // 滚动（否则鼠标往下滚遇到 mermaid 会变成缩图）。
        if self.focused_mermaid != Some(block) {
            return false;
        }
        let delta_y = match ev.delta {
            gpui::ScrollDelta::Lines(l) => l.y,
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y),
        };
        let step = delta_y.clamp(-1.0, 1.0);
        if step == 0.0 {
            return false;
        }
        self.zoom_mermaid_at(block, step, window, cx);
        true
    }
    /// mermaid 卡滚轮缩放 + **视口锚定**：改缩放（`mermaid_zoom_gen` 自增 → 帧/块高按新
    /// 比例失效）→ 渲染期重建窗口化帧 → 把「缩放前视口顶部的内容点」在新布局里的位置
    /// 写回宿主 `ScrollHandle`（同 `prepare_font_zoom` 的锚定恢复，一次性：mermaid 块
    /// 估计高与整形高同式 `×zoom`，无收敛过程）。未注入宿主句柄（独立测试）时只重建帧。
    pub(crate) fn zoom_mermaid_at(
        &self,
        block: usize,
        step: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 1) 重排前捕捉缩放锚点（当前视口顶部 → 块 + 块内比例）。
        self.capture_zoom_anchor();
        // 2) 应用新缩放（zgen++ → 帧缓存与基础布局代都失效）。
        let z = (self.mermaid_zoom(block) + step * 0.12).clamp(0.5, 3.0);
        self.set_mermaid_zoom(block, z);
        // 3) 渲染期重建窗口化帧 → 新布局的精确块 top/height。
        let width = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        let band = self.shape_band().unwrap_or((0., 5000.));
        let frame = element::build_frame(self, window, width, band);
        // 4) 锚点块在新布局里的精确位置 = 恢复目标（元素局部 y）。
        let target = {
            let anchor = *self.zoom_anchor.borrow();
            match anchor {
                Some((blk, frac)) => {
                    let count = frame.block_count();
                    let blk = blk.min(count.saturating_sub(1));
                    let h = frame.block_height(blk).max(1.);
                    if count > 0 {
                        frame.block_top(blk) + frac * h
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        };
        // 5) 写回宿主滚动句柄并按锚定后内容顶推导新带（与 element prepaint 同式）。
        let host = self.host_scroll.borrow().clone();
        let final_band = if let Some((vh, pad_y)) = host {
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
            zoom_band_for_top(target + pad_y, window.viewport_size().height)
        } else {
            band
        };
        let frame = if final_band != band {
            self.set_shape_band(Some(final_band));
            element::build_frame(self, window, width, final_band)
        } else {
            frame
        };
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        cx.notify();
    }
    /// 当前进行中的图片调整条拖拽的 src（测试）。
    #[cfg(test)]
    pub(crate) fn image_resize_drag_src(&self) -> Option<gpui::SharedString> {
        self.image_resize_drag
            .borrow()
            .as_ref()
            .map(|d| d.src.clone())
    }
    /// 正在拖拽图片调整条（paint 期据此保持 resize 光标）。
    pub(crate) fn image_resizing(&self) -> bool {
        self.image_resize_drag.borrow().is_some()
    }

    /// 命中调整条 → 开始拖拽：记录按下时的图片宽 + 指针窗口 x，并选中该图。
    fn begin_image_resize(&mut self, src: gpui::SharedString, start_w: f32, grab_x: f32) {
        *self.image_resize_drag.borrow_mut() = Some(ImageResizeDrag {
            src,
            start_w,
            grab_x,
        });
    }

    /// 拖拽中应用新宽：新宽 = `start_w + (mouse_x − grab_x)`（钳制），写回覆写表；
    /// 帧/整形缓存失效 → 图片随鼠标实时缩放（保持宽高比）。
    ///
    /// 坐标说明：`grab_x`（按下）与 `mouse_x`（移动）都取 **窗口 x**——两者同空间，
    /// 差值即鼠标横向位移，天然不受居中阅读栏水平移位（`reading_col_shift`）影响
    /// （移位在两项里同号抵消；且元素宽与帧宽在拖拽中不变，移位恒定）。
    fn apply_image_resize(&mut self, mouse_x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(d) = self.image_resize_drag.borrow().clone() else {
            return;
        };
        let new_w = d.start_w + (f32::from(mouse_x) - d.grab_x);
        self.set_image_width(d.src.clone(), new_w);
        *self.frame.borrow_mut() = None;
        cx.notify();
    }

    /// 结束调整条拖拽。
    fn end_image_resize(&mut self) {
        self.image_resize_drag.borrow_mut().take();
    }
}
