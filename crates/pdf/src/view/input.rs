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
use crate::pdf::parser::grid_bucket_width;

/// Windows 实时硬件态 Ctrl 探测：gpui 事件/window 修饰键在真机恒 false（GetKeyState
/// 线程消息队列态带不出），以 `GetAsyncKeyState(VK_CONTROL)` 读实时硬件态兜底——
/// 高位置位（bit 15 = 0x8000）即按下。非 Windows / **测试构建** 恒为 false。
///
/// 测试解耦：`GetAsyncKeyState` 读的是**整机实时键态**，测试进程无法空置它——
/// 若用户当下正按住 Ctrl（如刚在真机测完缩放），pdf_ctrl 类用例会把「普通滚轮」
/// 误判为 Ctrl+缩放而失败，并毒化全局 pdfium 锁引发整批级联失败。故 test 构建
/// 强制 false（纯逻辑断言不受真实键盘影响）；生产 exe 仍读真实硬件键。
#[cfg(all(target_os = "windows", not(test)))]
fn os_ctrl_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
    unsafe { GetAsyncKeyState(VK_CONTROL as i32) as i32 & 0x8000 != 0 }
}

#[cfg(all(target_os = "windows", test))]
fn os_ctrl_down() -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn os_ctrl_down() -> bool {
    false
}

/// 灰框/删除注释命中的点击容差（PDF 点，~4pt ≈ 2-3px@100%）：紧贴高亮边缘点击也应命中。
const ANNOT_HIT_TOL_PT: f32 = 4.0;
/// 图片命中点击容差（PDF 点，~1pt ≈ 0.7px@100%）：点图片描框线边缘也算命中；
/// 比注释容差更紧——图片是大块命中体，容差过大会把图旁紧邻的文字误判成图。
const IMAGE_HIT_TOL_PT: f32 = 1.0;

impl PdfViewer {
    // ---- 滚轮缩放 ----

    /// Ctrl+滚轮缩放：页列宽按倍率变化，锚定视口顶部内容点，重算布局。
    /// 瓦片不在此清空——回到可见页时 [`Slot::tile_scale`] 与新密度比对会惰性作废，
    /// 在途旧密度瓦片由 [`super::PdfViewer::collect_tile_results`] 的 scale 守卫丢弃。
    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 仅 Ctrl+滚轮缩放；普通滚轮不拦截（交给滚动容器）。
        //
        // Ctrl 判定 = 事件自带修饰键 ∪ 窗口侧当前修饰键 ∪ Windows 实时硬件键（三重并集）：
        // gpui_windows 给每个滚轮事件附的 modifiers 来自 `current_modifiers()`
        // （= GetKeyState(VK_CONTROL)，gpui_windows/events.rs:1751），它反映的是
        // 线程消息队列态而非实时硬件态，真机曾实测恒 false；窗口侧公开 API
        // `Window::modifiers()`（gpui/src/window.rs:3405，pub fn）在真机同样恒 false；
        // 故再并入 [`os_ctrl_down`]（Windows 走 GetAsyncKeyState 读实时硬件态）兜底，
        // 任一来源为 true 即按 Ctrl 处理。普通滚轮保持不拦截（不调 stop_propagation）。
        let event_ctrl = event.modifiers.control;
        let win_ctrl = window.modifiers().control;
        let os_ctrl = os_ctrl_down();
        let ctrl = event_ctrl || win_ctrl || os_ctrl;
        if !ctrl {
            if timing_view() {
                eprintln!(
                    "[{:.1}ms] [VIEW-WHEEL] 普通滚轮 ev_ctrl={} win_ctrl={} os_ctrl={} 不缩放",
                    crate::pdf::timing_ms(),
                    event_ctrl,
                    win_ctrl,
                    os_ctrl,
                );
            }
            return;
        }
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-WHEEL] Ctrl ev_ctrl={} win_ctrl={} os_ctrl={} delta_y={:.0} zoom_before={:.2}",
                crate::pdf::timing_ms(),
                event_ctrl,
                win_ctrl,
                os_ctrl,
                pan_zoom::wheel_delta_y(event),
                self.zoom,
            );
        }
        let delta_y = pan_zoom::wheel_delta_y(event);
        let factor = pan_zoom::zoom_factor(delta_y, ZOOM_STEP);
        if self.grid_mode {
            // 网格**独立缩放**：只改 `grid_zoom` → 卡宽/列数变化（`grid_card_width()` 派生），
            // **不碰单列阅读 `self.zoom`**（退出网格回阅读保持用户阅读缩放）。不复用
            // `adjust_zoom`（那会改 `self.zoom` + 重排单列布局 + 步进 zoom_epoch）；这里只
            // 步进 grid_zoom 并同比重锚 grid_scroll。缩略图**仅跨分桶才失效**——worker 按
            // `grid_bucket_width`（GRID_BUCKET_PX=32 量化）分桶缓存，同一桶内缩放只改卡宽/
            // 列数/滚动重锚，缩略图与在途请求**复用**（连续 Ctrl+滚轮跨多桶时每档全清会
            // 闪占位 + 逐档重栅）；同桶复用、跨桶失效（下一帧按新分桶重取）。
            let old_card = self.grid_card_width();
            if pan_zoom::apply_zoom(&mut self.grid_zoom, factor, ZOOM_MIN, ZOOM_MAX) {
                let new_card = self.grid_card_width();
                if (new_card - old_card).abs() > 1e-3 {
                    let ratio = new_card / old_card;
                    let sy = f32::from(-self.grid_scroll.offset().y) * ratio;
                    self.grid_scroll.set_offset(gpui::point(px(0.), px(-sy)));
                    if grid_bucket_width(new_card) != grid_bucket_width(old_card) {
                        self.clear_grid_resources();
                    }
                    cx.notify();
                }
            }
        } else {
            self.adjust_zoom(factor, cx);
        }
        cx.stop_propagation();
        if timing_view() {
            eprintln!(
                "[{:.1}ms] [VIEW-WHEEL] adjust_zoom 后 zoom={:.2} page_width={:.0}",
                crate::pdf::timing_ms(),
                self.zoom,
                self.page_width(),
            );
        }
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
        let ratio = new_w / old_w;
        // Z2 缩放世代步进（view 唯一真源）→ 推送 worker 采纳镜像。渲染请求随后 stamp 新世代；
        // 停在 worker 队列的缩放**中间档** cover/瓦片/preview 出队时 `stamp < mirror` → 直接
        // drop 不渲（旧档不再排队空耗 pdfium 时隙；最终档必达、无中间档堆叠）。
        self.zoom_epoch += 1;
        if let Some(w) = self.worker.clone() {
            w.set_zoom_epoch(self.doc_id, self.zoom_epoch);
        }
        // Z1 锚点重锚：页列布局随 page_width 线性缩放但 `PAGE_PAD_Y`/`PAGE_GAP` 常量不缩放
        // → 旧实现「整个 offset × ratio」把常量区也当线性量乘，每档漂移
        // `-(ratio-1)×(PAD+K×GAP)`（K=锚点页号，深页可跳 2-3 行高，放大上跳/缩小下跳离散跳）。
        // 改为「锚定 scroll 落在的页 K：页顶线性缩放 + 页内相对偏移同比缩放」（纯函数
        // [`layout::zoom_rescroll_anchor`] 锁死公式）。横向量列居中同款同比缩放。
        let sx = f32::from(-self.scroll.offset().x);
        let sy = f32::from(-self.scroll.offset().y);
        let old_tops = std::mem::take(&mut self.page_tops);
        let (new_tops, total) = compute_layout(&self.dims, new_w);
        self.page_tops = new_tops;
        self.total_height = total;
        let new_y = zoom_rescroll_anchor(&self.dims, &old_tops, &self.page_tops, old_w, ratio, sy);
        // #2 停底锚点回拉修正：Z1 公式对页尾/文档底附近给出的 `new_y` 可能超出新布局的
        // `[0, max_offset]` ——gpui 只在**布局**时才 clamp（div.rs `clamp_scroll_position`），
        // 本帧读到的是未夹值：视觉已停底、`last_scroll_y` 却记了越界假值 → 下一帧
        // `ensure_window` 算 jump≈「max−越界」假速度（S1 复核期失真）+ 再放大反向补一跳。
        // 这里按新布局量程先 clamp，并把 `last_scroll_y` 回写为 clamp 后值（防 S1 假速度），
        // 横向量程同款（内容宽 = 可视宽与页宽+边距的较大者，`zoom==1` 适配宽时恒 0）。
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let max_sy = (self.total_height - viewport_h).max(0.0);
        let new_y = new_y.clamp(0.0, max_sy);
        let viewport_w = f32::from(self.scroll.bounds().size.width);
        let content_w = viewport_w.max(new_w + 2.0 * PAGE_PAD_X);
        let max_sx = (content_w - viewport_w).max(0.0);
        let new_x = (sx * ratio).clamp(0.0, max_sx);
        self.scroll
            .set_offset(gpui::point(px(-new_x), px(-new_y)));
        // S1 缩放源头治理：重锚把 scroll offset 同比缩放后，滚动速度「账面」与滚动锚点立即
        // 同步——下一帧 `ensure_window` 的 `jump = scroll_y - last_scroll_y ≈ 0`，不再把缩放
        // 重锚误判为滚动（旧实现：同帧 offset 突变 → velocity 飙 → VeryFast → 手势期零派发
        // + 2-4 帧「加载中」空白 + 屏显恒为手势前旧封面 1.12^k 复利拉伸）。
        // 强制 Slow 保证该缩放事件帧不被高速带缠住；`zoom_armed` 置位令缩放防抖窗内调度
        // 只派当前档 preview（绝不满密 cover/tile 逐档抢渲被弃空转），防抖到期才满密收口。
        // `last_scroll_y` = clamp 后的 `new_y`（#2：与写入 scroll 的 offset 一致，防 S1 假速度）。
        self.last_scroll_y = new_y;
        self.scroll_velocity = 0.0;
        self.speed_state = SpeedState::Slow;
        self.prev_fast = false;
        self.zoom_armed = true;
        self.zoom_idle_frames = 0;
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

    /// 是否**横向溢出**（内容宽于视口、可横向平移）。两条件缺一不可：
    /// - `max_offset().x > 1px`：内容确实宽于视口（防 0.x 的舍入抖动）；
    /// - `zoom > 1`：**放大**产生的溢出才是用户可感知的「被截断」。适配宽（`zoom==1`）
    ///   时页宽基线随容器收敛，哪怕容器临时窄于页宽+边距（窗口回退基线未收敛的瞬时
    ///   ~46px 级溢出），也仍按「选中文本」处理——否则 shell b1 在打开头几帧即拖选会
    ///   被误切进平移（选中路径回归）。
    /// 溢出时左键拖拽语义从「选中文本」切换为「平移页面」（行为表见 [`Self::on_mouse_down`]）。
    pub(crate) fn horizontal_overflow(&self) -> bool {
        self.scroll.max_offset().x > px(1.) && self.zoom > 1.0 + 1e-4
    }
}

impl PdfViewer {
    // ---- P2 文本层：命中测试 / 选区 / 复制 / 懒加载缓存 ----

    /// 窗口坐标 → 页内 PDF 点 `(page, px_pdf, py_pdf)`（y up）。坐标链与 `hit_test`
    /// 同源（窗口 → 滚动局部 → wrap → 列 → `content_to_local` 定页 → `local_to_pt` 还原
    /// PDF 点）；点空白（列外/页间沟槽）返回 `None`。`hit_test` 在其上再叠 `hit_char`
    /// 定字符；注释命中（灰框）直接用它判定点是否落在 pending quads 内、无需文本层。
    pub(super) fn window_to_pt(
        &self,
        window: &Window,
        pos: Point<Pixels>,
    ) -> Option<(usize, f32, f32)> {
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
        let scale = render_scale(page_width, window.scale_factor(), self.dims[page].0);
        let (px_, py_) =
            local_to_pt(lx, ly, self.dims[page].0, self.dims[page].1, page_width, scale);
        Some((page, px_, py_))
    }

    /// 窗口坐标 → (页号, 字符索引)。点空白（列外/页间沟槽/无文本页）返回 `None`。
    pub(super) fn hit_test(&self, window: &Window, pos: Point<Pixels>) -> Option<(usize, usize)> {
        let (page, px_, py_) = self.window_to_pt(window, pos)?;
        let text = self.text_pages.get(&page)?;
        hit_char(text, px_, py_).map(|ci| (page, ci))
    }

    // ---- 图片命中 / 选中 / 「复制图片」取数（P 图片对象交互）----
    //
    // 与文本层同款坐标链（`window_to_pt` → 页内 PDF 点 → `image_pages` 页图片盒），
    // 页图片盒由 [`super::PdfViewer::prefetch_images`] 与文本层同节奏懒加载预取；
    // 未缓存/未命中 → `None`（回落到文本划选语义）。

    /// 窗口坐标 → (页号, 对象下标)：点落在某页某张图片盒内（1pt 容差）→ 取**该页最上层**
    /// （对象顺序最靠后）命中盒；否则 `None`。页图片盒未缓存（预取未落地）视为未命中。
    pub fn image_at_window_pos(
        &self,
        window: &Window,
        pos: Point<Pixels>,
    ) -> Option<(usize, usize)> {
        let (page, px_, py_) = self.window_to_pt(window, pos)?;
        let boxes = self.image_pages.get(&page)?;
        // 倒序找最上层：后绘制图片覆盖先绘制者（点击上层命中上层，同注释最近优先精神）。
        boxes
            .iter()
            .rev()
            .find(|b| b.contains_pt_tol(px_, py_, IMAGE_HIT_TOL_PT))
            .map(|b| (page, b.obj_index))
    }

    /// 图片盒 → **窗口坐标**（盒中心；测试探针/右键命中定位用）。页图片盒未缓存或
    /// 对象下标不存在 → `None`。坐标链与 [`Self::char_window_pos`] 同源（`pt_to_local`
    /// 翻转 → `page_local_to_window`）。
    pub fn image_pos(
        &self,
        window: &Window,
        page: usize,
        obj_index: usize,
    ) -> Option<gpui::Point<Pixels>> {
        if page >= self.dims.len() {
            return None;
        }
        let b = self.image_pages.get(&page)?.iter().find(|b| b.obj_index == obj_index)?;
        let dim = self.dims[page];
        let page_width = self.page_width();
        let scale = render_scale(page_width, window.scale_factor(), dim.0);
        let (lx0, ly0) = pt_to_local(b.x0, b.y1, dim.0, dim.1, page_width, scale); // (left, top → 小 y)
        let (lx1, ly1) = pt_to_local(b.x1, b.y0, dim.0, dim.1, page_width, scale); // (right, bottom → 大 y)
        Some(self.page_local_to_window(
            window,
            page,
            (lx0 + lx1) / 2.0,
            (ly0 + ly1) / 2.0,
        ))
    }

    /// 页图片盒访问器（shell 测试/定位用）：页未缓存（预取未落地）→ `None`；
    /// 页已缓存但无图 → `Some(&[])`。返回的盒为 PDF 点、y 向上。
    pub fn image_boxes(&self, page: usize) -> Option<&[PdfImageBox]> {
        self.image_pages.get(&page).map(Vec::as_slice)
    }

    /// 把某图置为选中（右键「未选中则先选中」/ 测试直接驱动）。不触碰文本选区
    /// （互斥只发生在**左键**点击路径，右键仅选中图片、保留文本选区语义）。
    pub fn select_image(&mut self, page: usize, obj_index: usize, cx: &mut Context<Self>) {
        if self.selected_image != Some((page, obj_index)) {
            self.selected_image = Some((page, obj_index));
            cx.notify();
        }
    }

    /// 触发「复制图片」取数：按选中图**自然分辨率**经 worker 取紧凑 BGRA 像素，
    /// 返回结果通道（oneshot：`Some(PdfImagePixels)` / 无选中/对象失效/取位图失败
    /// `None`）。取数是**按需一次性**、不缓存像素（大图驻留浪费，只过一手剪贴板）。
    pub fn request_image_pixels(
        &self,
        page: usize,
        obj_index: usize,
    ) -> Option<mpsc::Receiver<Option<PdfImagePixels>>> {
        let worker = self.worker.as_ref()?;
        let boxes = self.image_pages.get(&page)?;
        let b = boxes.iter().find(|b| b.obj_index == obj_index)?;
        let w = b.natural_w.max(1);
        let h = b.natural_h.max(1);
        Some(worker.image_pixels(self.doc_id, page, obj_index, w, h))
    }

    /// 鼠标按下：聚焦（供 Ctrl+C）。**同时关闭选中弹卡**（点卡片外 / 重新开始拖选 /
    /// 点击 PDF 任意处都会先走到这里——卡片在独立浮层上，卡片自身点击不会冒泡到此）。
    /// 命中 → 置锚点/活动端并进入拖拽；点空白 → 清空选区。
    ///
    /// **左键拖拽行为表**（超宽 vs 适配宽）：
    /// - 未横向溢出（正常适配宽度）：按下 → 命中选中起点；拖拽 → 扩展选区；
    ///   松手 → 弹选中菜单（既有语义，完整保留）。
    /// - 横向溢出（Ctrl+滚轮放大后内容宽于视口）：按下 → 进入平移（`panning=true`）；
    ///   拖拽 → 页面跟手移动（`on_mouse_move`）；松手 → 结束平移（无选中、无菜单）。
    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        self.sel_card = None;
        if event.button != MouseButton::Left {
            return;
        }
        // 超宽平移：左键按下即进入平移（不依赖命中文本），不进选中。
        if self.horizontal_overflow() {
            self.panning = true;
            self.pan_origin = event.position;
            self.pan_offset0 = self.scroll.offset();
            self.selecting = false;
            cx.notify();
            return;
        }
        // 图片命中优先于文本划选：左键单击页内图片 → 选中该图（描框高亮）+ **互斥清除**
        // 文本选区（不回落到拖选起点、不弹卡）。
        if let Some(img) = self.image_at_window_pos(window, event.position) {
            self.selected_image = Some(img);
            self.sel_anchor = None;
            self.sel_active = None;
            self.selecting = false;
            cx.notify();
            return;
        }
        // 未命中图片（点文字/空白）：清除图片选中（文本选区逻辑不变）。
        if self.selected_image.take().is_some() {
            cx.notify();
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
    /// 平移模式（超宽）：滚动偏移 = 按下时快照 + 指针位移（页面跟手，`window.mouse_position`
    /// 比拖拽事件 position 即时——gpui 移动事件可被上层消费，mouse_position 是真实光标）。
    pub(super) fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.panning {
            let delta = window.mouse_position() - self.pan_origin;
            self.scroll.set_offset(self.pan_offset0 + delta);
            cx.notify();
            return;
        }
        // 兜底自愈：`selecting` 臂若因任一路径漏配 `mouse_up` 而滞留（菜单/弹层关闭、
        // 事件被上层 stop_propagation 吃掉等），下一次「未按住左键」的移动即复位——
        // 防止菜单点击后无按键移动被误当拖选。正常拖选期间移动事件携带
        // `pressed_button=Left`（`dragging()` 为真）不触发；松手由 `on_mouse_up` 正常复位。
        if self.selecting && !event.dragging() {
            self.selecting = false;
            cx.notify();
            return;
        }
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
    /// Phase B1：若是一次**真实拖选**（松手时光标 ≠ 按下点，即选区非空）→ 在**松开鼠标处**
    /// 弹操作菜单（复制/高亮/删除线/下划线，同右键菜单定位在点击处），并清「灰框」焦
    /// （选区高亮接管）；单击（锚==活动）不弹卡 → **灰框命中**：点在待存注释 quads 内 →
    /// 框住该注释文字（点非注释文字 → 清灰框）。
    pub(super) fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 平移模式松手：结束平移、不弹选中菜单。放最前（先于 `if !self.selecting`）。
        if self.panning {
            self.panning = false;
            self.selecting = false;
            cx.notify();
            return;
        }
        if !self.selecting {
            return;
        }
        self.selecting = false;
        let dragged = self.selection().is_some_and(|(a, b)| a != b);
        if dragged {
            self.focused_annot = None;
            // 菜单位于松手处（窗口坐标，锚点 TopLeft，越界由渲染层 snap 夹回）——
            // 与右键菜单「点在哪出在哪」的交互一致，而非选区几何中心（K39）。
            self.sel_card = Some(SelCard {
                pos: event.position,
                scroll: self.scroll.offset(),
            });
        } else {
            // 单击：命中待存注释 → 置灰框；点非注释文字 → 清灰框
            let hit = self
                .window_to_pt(window, event.position)
                .and_then(|(page, px, py)| self.pending_annot_at_pt(page, px, py));
            if self.focused_annot != hit {
                self.focused_annot = hit;
            }
        }
        cx.notify();
    }

    /// 快捷键：`Ctrl/Cmd+C` 复制选区；`Ctrl/Cmd+Z` 撤销上一条待存编辑（注释/页面操作）
    /// 且 stop_propagation 不再冒泡给全局编辑器 Undo；`Esc` 关闭选中弹卡。
    pub(super) fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.keystroke.modifiers;
        if (m.control || m.platform) && !m.alt && !m.shift && event.keystroke.key == "c" {
            self.copy_selection(window, cx);
        }
        if (m.control || m.platform) && !m.alt && !m.shift && event.keystroke.key == "z"
            && !self.undo_stack.is_empty()
        {
            self.undo_last(cx);
            cx.stop_propagation();
        }
        if event.keystroke.key == "escape" && self.sel_card.is_some() {
            self.sel_card = None;
        }
        // wave6：网格拖拽中 Esc → 取消拖拽（放弃提交、选中保持原样），并 stop_propagation
        // 不冒泡给全局（避免其它 Escape 消费方误处理）。
        if event.keystroke.key == "escape" && self.grid_drag.is_some() {
            self.grid_drag_cancel(cx);
            cx.stop_propagation();
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
    /// `pub`：Phase B 选中弹卡「复制」与 shell 测试共享同一实现。
    pub fn selected_text(&self) -> Option<String> {
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

    // ---- Phase B1：选中弹卡（坐标几何 + 生命周期 + 注释写回）----

    /// 页卡局部 `(page, lx, ly)` → **窗口坐标**。坐标链与 [`Self::hit_test`] 同源互逆：
    /// 页卡局部 → 内容(wrap)坐标（加 [`column_left`] / `page_tops`）→ 窗口坐标
    /// （加滚动容器原点与本帧滚动偏移）。`SelCard` 锚点与 `char_window_pos` 共用。
    pub(super) fn page_local_to_window(
        &self,
        window: &Window,
        page: usize,
        lx: f32,
        ly: f32,
    ) -> gpui::Point<Pixels> {
        let page_width = self.page_width();
        let wrap_w = self.wrap_width(window, page_width);
        let bounds = self.scroll.bounds();
        let off = self.scroll.offset();
        let cx = column_left(wrap_w, page_width) + lx;
        let cy = self.page_tops.get(page).copied().unwrap_or(0.0) + ly;
        gpui::point(
            px(bounds.left().as_f32() + cx + off.x.as_f32()),
            px(bounds.top().as_f32() + cy + off.y.as_f32()),
        )
    }

    /// 字符索引 → **窗口坐标**（字符盒中心；`page` 文本未缓存 / 索引越界 → `None`）。
    /// Phase B2 测试/探针定位拖选起点用（shell `app::tests` 经真实拖选驱动弹卡）。
    pub fn char_window_pos(
        &self,
        window: &Window,
        page: usize,
        idx: usize,
    ) -> Option<gpui::Point<Pixels>> {
        if page >= self.dims.len() {
            return None;
        }
        let text = self.text_pages.get(&page)?;
        let c = text.chars.get(idx)?;
        if c.synthetic || c.right <= c.left || c.top <= c.bottom {
            return None;
        }
        let dim = self.dims[page];
        let page_width = self.page_width();
        let scale = render_scale(page_width, window.scale_factor(), dim.0);
        let (lx0, ly0) = pt_to_local(c.left, c.top, dim.0, dim.1, page_width, scale);
        let (lx1, ly1) = pt_to_local(c.right, c.bottom, dim.0, dim.1, page_width, scale);
        Some(self.page_local_to_window(
            window,
            page,
            (lx0 + lx1) / 2.0,
            (ly0 + ly1) / 2.0,
        ))
    }

    /// 选区包围盒（**窗口坐标**）：逐页把 [`selection_spans_pt`] 的 span 转窗口坐标取并集。
    /// 缺文本页跳过（与 `selected_text` 尽力而为口径一致）；无任何 span → `None`。
    fn selection_window_bounds(
        &self,
        window: &Window,
    ) -> Option<((f32, f32), (f32, f32))> {
        let ((ap, ac), (bp, bc)) = self.selection()?;
        let page_width = self.page_width();
        let scale_factor = window.scale_factor();
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for page in ap..=bp {
            let Some(text) = self.text_pages.get(&page) else {
                continue;
            };
            let dim = self.dims.get(page)?;
            let n = text.chars.len();
            if n == 0 {
                continue;
            }
            let lo = if page == ap { ac.min(n - 1) } else { 0 };
            let hi = if page == bp { bc.min(n - 1) } else { n - 1 };
            if lo > hi {
                continue;
            }
            let scale = render_scale(page_width, scale_factor, dim.0);
            for (l, b, r, t) in selection_spans_pt(text, lo, hi) {
                let (x0, y_top) = pt_to_local(l, t, dim.0, dim.1, page_width, scale);
                let (x1, y_bottom) = pt_to_local(r, b, dim.0, dim.1, page_width, scale);
                let tl = self.page_local_to_window(window, page, x0.min(x1), y_top.min(y_bottom));
                let br = self.page_local_to_window(window, page, x0.max(x1), y_top.max(y_bottom));
                min_x = min_x.min(f32::from(tl.x));
                min_y = min_y.min(f32::from(tl.y));
                max_x = max_x.max(f32::from(br.x));
                max_y = max_y.max(f32::from(br.y));
            }
        }
        (min_x.is_finite()).then_some(((min_x, min_y), (max_x, max_y)))
    }

    /// 弹卡锚点（**仅测试探针用**：无真实鼠标位置可取）：选区包围盒**上缘中心**上方一点。
    /// 真实拖选路径已改用「松手处窗口坐标」（`on_mouse_up` 的 `event.position`）。
    /// 取不到包围盒 → `None`。
    pub(super) fn selection_card_anchor(&self, window: &Window) -> Option<gpui::Point<Pixels>> {
        let ((min_x, min_y), (max_x, _)) = self.selection_window_bounds(window)?;
        Some(gpui::point(
            px((min_x + max_x) / 2.0),
            px(min_y - 8.0),
        ))
    }

    /// 关闭选中弹卡（点卡片外 / 重新拖选 / 滚动 / Esc 均走此收敛）。
    pub(super) fn close_sel_card(&mut self, cx: &mut Context<Self>) {
        if self.sel_card.take().is_some() {
            cx.notify();
        }
    }

    /// 把当前选区逐页转 PDF pt quad（y 向上），合并成**一次**批量注释写回源文件（跨页
    /// 也只一次落盘 + 一次重载，见 [`super::open::PdfViewer::submit_annotations`] 的竞态
    /// 说明）；完成后清选区 + 关卡。缺文本页跳过（尽力而为）。
    pub(super) fn annotate_selection(&mut self, kind: AnnotKind, cx: &mut Context<Self>) {
        let Some(((ap, ac), (bp, bc))) = self.selection() else {
            return;
        };
        let mut items: Vec<AnnotateItem> = Vec::new();
        for page in ap..=bp {
            let Some(text) = self.text_pages.get(&page).cloned() else {
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
            if page >= self.dims.len() {
                continue;
            }
            let quads: Vec<QuadPt> = selection_spans_pt(&text, lo, hi)
                .into_iter()
                .map(|(l, b, r, t)| QuadPt { x0: l, y0: b, x1: r, y1: t })
                .collect();
            if quads.is_empty() {
                continue;
            }
            items.push(AnnotateItem {
                page_index: page,
                kind,
                quads,
            });
        }
        if items.is_empty() {
            return;
        }
        self.submit_annotations(items, cx);
        // 写完即清选区与卡片（注释已上屏，选区高亮不再保留）。
        self.sel_anchor = None;
        self.sel_active = None;
        self.sel_card = None;
        cx.notify();
    }

    /// 页内 PDF 点是否落在某条待存注释的 quads 内（含点击容差 [`ANNOT_HIT_TOL_PT`]）。
    /// 多条叠实时取**最近添加**的一条（用户通常点最新高亮）。
    fn pending_annot_at_pt(&self, page: usize, px: f32, py: f32) -> Option<u64> {
        self.pending_annots
            .iter()
            .rev()
            .filter(|a| a.page == page)
            .find(|a| {
                a.quads_pt.iter().any(|q| {
                    let (x0, y0) = (q.x0.min(q.x1), q.y0.min(q.y1));
                    let (x1, y1) = (q.x0.max(q.x1), q.y0.max(q.y1));
                    px >= x0 - ANNOT_HIT_TOL_PT && px <= x1 + ANNOT_HIT_TOL_PT
                        && py >= y0 - ANNOT_HIT_TOL_PT && py <= y1 + ANNOT_HIT_TOL_PT
                })
            })
            .map(|a| a.id)
    }

    /// 右键/点击命中待存注释：`pos` 落在某条注释 quads 内 → 置灰框焦并返回 id；
    /// 否则清灰框返回 `None`。壳层右键菜单据此决定是否追加「删除注释」。
    pub fn focus_annot_at_window_pos(
        &mut self,
        window: &Window,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let hit = self
            .window_to_pt(window, pos)
            .and_then(|(page, px, py)| self.pending_annot_at_pt(page, px, py));
        if self.focused_annot != hit {
            self.focused_annot = hit;
            cx.notify();
        }
        hit
    }

    /// 测试探针（shell `app::tests` 用）：跳过真实拖选几何，直接把选区设为 `(lo, hi)`
    /// 并弹卡——等价于「拖选到 lo→hi 后松手」。真实拖选状态机路径由
    /// `char_window_pos + simulate_mouse_*` 的端到端用例另盖。
    pub fn open_selection_card_for_test(
        &mut self,
        lo: (usize, usize),
        hi: (usize, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        self.sel_anchor = Some(lo);
        self.sel_active = Some(hi);
        self.selecting = false;
        if let Some(pos) = self.selection_card_anchor(window) {
            self.sel_card = Some(SelCard {
                pos,
                scroll: self.scroll.offset(),
            });
        }
        cx.notify();
    }
}
