//! PDF 阅读器子模块：交互 / 命中 / 选区（原 `view.rs` 拆出）。
//!
//! - **滚轮缩放**：Ctrl+滚轮经 [`viewer::pan_zoom`] 按 [`config::pdf`] 区间钳位缩放，
//!   锚定视口顶部内容点、重算页列布局（[`super::PdfViewer::adjust_zoom`]）；
//! - **命中测试**（[`super::PdfViewer::hit_test`]）：窗口 → 滚动 → wrap → 列坐标链
//!   定位 (页, 字符索引)，与瓦片的 `page_visible_rect` 用同一套坐标链；
//! - **选区 / 复制**：左键拖拽选区，`Ctrl/Cmd+C` 经 [`super::PdfViewer::selected_text`]
//!   写系统剪贴板（缺页文本跳过，尽力而为）。
//!
//! 交互挂载点在 `render` 子模块（`on_mouse_down/up/move`、`on_key_down`、
//! `on_scroll_wheel` 均经 `cx.listener` 注册到页列滚动容器）；私有成员经
//! `` `pub(super)` ``（视图子树内可见）互调。

use super::*;

impl PdfViewer {
    // ---- 滚轮缩放 ----

    /// Ctrl+滚轮缩放：页列宽按倍率变化，锚定视口顶部内容点，重算布局。
    /// 瓦片不在此清空——回到可见页时 [`Slot::tile_scale`] 与新密度比对会惰性作废，
    /// 在途旧密度瓦片由 [`super::PdfViewer::collect_tile_results`] 的 scale 守卫丢弃。
    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 仅 Ctrl+滚轮缩放；普通滚轮不拦截（交给滚动容器）。
        if !event.modifiers.control {
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-WHEEL] 普通滚轮 control=false 不缩放",
                    crate::pdf::timing_ms()
                );
            }
            return;
        }
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-WHEEL] Ctrl control=true delta_y={:.0} zoom_before={:.2}",
                crate::pdf::timing_ms(),
                pan_zoom::wheel_delta_y(event),
                self.zoom,
            );
        }
        let delta_y = pan_zoom::wheel_delta_y(event);
        // 实测本平台：向上滚动 delta.y>0 → 放大；向下 <0 → 缩小（与编辑器字号缩放一致）
        self.adjust_zoom(pan_zoom::zoom_factor(delta_y, ZOOM_STEP), cx);
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-WHEEL] adjust_zoom 后 zoom={:.2} page_width={:.0}",
                crate::pdf::timing_ms(),
                self.zoom,
                self.page_width(),
            );
        }
        cx.stop_propagation();
    }

    /// 应用缩放因子并维护其后的一致性：锚定滚动偏移、重算页列布局。钳位/无变化短路走
    /// [`pan_zoom::apply_zoom`]（区间 [ZOOM_MIN..=ZOOM_MAX] 来自 [`config::pdf`]）。
    pub(super) fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if self.dims.is_empty() {
            return;
        }
        let old_w = self.page_width();
        if !pan_zoom::apply_zoom(&mut self.zoom, factor, ZOOM_MIN, ZOOM_MAX) {
            return;
        }
        let new_w = self.page_width();
        // 锚定视口：页列布局随 page_width 线性缩放 → 同一文档点的滚动偏移同比缩放。
        let ratio = new_w / old_w;
        let sx = f32::from(-self.scroll.offset().x);
        let sy = f32::from(-self.scroll.offset().y);
        self.scroll
            .set_offset(gpui::point(px(-sx * ratio), px(-sy * ratio)));
        // 重算布局：瓦片按新密度惰性重渲（见 `schedule_tiles` 的 tile_scale 比对）。
        let (tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = tops;
        self.total_height = total;
        cx.notify();
    }

    /// 页列 wrap 宽度 = 滚动容器实际可视宽。⚠️ 不用 `window.bounds()`（那是全窗口宽）：
    /// 侧边栏开启时可视区比窗口窄，若 wrap 按窗口宽居中，页列会相对可视区整体右偏。
    /// 首帧未布局时 `scroll.bounds()` 宽为 0，回退到窗口宽——只影响首帧，布局后即校正。
    pub(super) fn wrap_width(&self, window: &Window, page_width: f32) -> f32 {
        let view = f32::from(self.scroll.bounds().size.width);
        let view = if view > 0.0 {
            view
        } else {
            window.bounds().size.width.as_f32()
        };
        view.max(page_width + 2.0 * PAGE_PAD_X)
    }
}

impl PdfViewer {
    // ---- P2 文本层：命中测试 / 选区 / 复制 / 懒加载缓存 ----

    /// 窗口坐标 → (页号, 字符索引)。点空白（列外/页间沟槽/无文本页）返回 `None`。
    ///
    /// 坐标链：窗口坐标 → 滚动容器局部 → 内容(wrap)坐标 → 列坐标（减 [`column_left`]）
    /// → `content_to_local` 定页 + 页内局部 → `local_to_pt` 逆映射回 PDF 点 → `hit_char`。
    pub(super) fn hit_test(&self, window: &Window, pos: Point<Pixels>) -> Option<(usize, usize)> {
        if self.dims.is_empty() {
            return None;
        }
        let bounds = self.scroll.bounds();
        let mx = pos.x.as_f32() - bounds.left().as_f32();
        let my = pos.y.as_f32() - bounds.top().as_f32();
        let off = self.scroll.offset();
        let wx = mx - off.x.as_f32();
        let wy = my - off.y.as_f32();
        let page_width = self.page_width();
        let wrap_w = self.wrap_width(window, page_width);
        let cx = wx - column_left(wrap_w, page_width);
        let (page, lx, ly) = content_to_local(&self.dims, &self.page_tops, page_width, cx, wy)?;
        let text = self.text_pages.get(&page)?;
        let scale = render_scale(page_width, window.scale_factor(), self.dims[page].0);
        let (px_, py_) =
            local_to_pt(lx, ly, self.dims[page].0, self.dims[page].1, page_width, scale);
        hit_char(text, px_, py_).map(|ci| (page, ci))
    }

    /// 鼠标按下：聚焦（供 Ctrl+C）。命中 → 置锚点/活动端并进入拖拽；点空白 → 清空选区。
    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if event.button != MouseButton::Left {
            return;
        }
        match self.hit_test(window, event.position) {
            Some(pos) => {
                self.sel_anchor = Some(pos);
                self.sel_active = Some(pos);
                self.selecting = true;
            }
            None => {
                self.sel_anchor = None;
                self.sel_active = None;
                self.selecting = false;
            }
        }
        cx.notify();
    }

    /// 按住左键拖拽 → 活动端随鼠标更新（选区实时扩展/收缩）。
    pub(super) fn on_mouse_move(&mut self, _: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let pos = window.mouse_position();
        if let Some(cur) = self.hit_test(window, pos)
            && self.sel_active != Some(cur) {
                self.sel_active = Some(cur);
                cx.notify();
            }
    }

    /// 松开：拖拽结束（`selecting` 复位），**选区保留**（anchor + active 都在，可 Ctrl+C）。
    pub(super) fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            cx.notify();
        }
    }

    /// 快捷键：`Ctrl/Cmd+C` 复制选区。（同 markdown 预览的按键处理。
    /// 全文检索已迁移到侧边栏搜索框，PDF 内不再自建搜索框。）
    pub(super) fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.keystroke.modifiers;
        if (m.control || m.platform) && !m.alt && !m.shift && event.keystroke.key == "c" {
            self.copy_selection(window, cx);
        }
        cx.notify();
    }

    /// 把当前选区文本写入系统剪贴板。
    pub(super) fn copy_selection(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// 归一化选区（锚点 ≤ 活动端，页号与字符索引均按此序）。
    pub(super) fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.sel_anchor?;
        let b = self.sel_active?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// 选区文本：逐页取**已缓存**文本（缺页跳过，尽力而为），跨页以 `\n` 拼接。
    pub(super) fn selected_text(&self) -> Option<String> {
        let ((ap, ac), (bp, bc)) = self.selection()?;
        let mut out = String::new();
        for page in ap..=bp {
            let Some(text) = self.text_pages.get(&page) else {
                continue;
            };
            let n = text.chars.len();
            if n == 0 {
                continue;
            }
            let lo = if page == ap { ac.min(n - 1) } else { 0 };
            let hi = if page == bp { bc.min(n - 1) } else { n - 1 };
            if lo > hi {
                continue;
            }
            let seg = page_segment_text(text, lo, hi);
            if seg.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&seg);
        }
        (!out.is_empty()).then_some(out)
    }
}
