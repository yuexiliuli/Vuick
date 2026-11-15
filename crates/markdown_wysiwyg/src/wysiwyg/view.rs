use super::*;

// 区域二分定位（`region_containing`/`folded_body_at`）已随 `fold_plan` 迁入引擎
// `frame` 模块；宿主 `edit.rs`/`math_edit.rs` 的 `super::view::region_containing` 引用
// 原样成立。
pub(crate) use markdown_wysiwyg_engine::frame::region_containing;

/// WyEditor 滚动 / 缩放 / 布局状态 / 折叠计划 / notdef 桥接（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /// 缩放锚点收敛判定：相邻两帧恢复目标位移小于此值（px）即视为高度已稳定并消费锚点。
    pub(crate) const ZOOM_ANCHOR_EPS: f32 = 0.5;
    /// 缩放锚点收敛兜底：最多逐帧施加这么多次后强制消费（防高度持续细化的极端文档上
    /// 锚点永不收敛、宿主每帧覆盖用户滚动）。
    pub(crate) const ZOOM_ANCHOR_MAX_APPLIES: u32 = 30;
    /// A4：目录/搜索跳转 —— 把（只读）预览滚动到**源码第 `src_line` 行**所在块。
    /// 镜像 `prepare_font_zoom` 渲染期重建套路：先以当前（或默认带建的）窗口化帧估块顶
    /// → 定位目标可视带 → 在目标带重建窗口化帧读**精确**块顶 → 写回宿主 `ScrollHandle`
    ///（内容坐标 top = −offset.y − pad_y → offset = −(top + pad_y)），并发布新带给下帧。
    pub fn scroll_to_line(&mut self, src_line: usize, window: &mut Window) {
        let _ = self.scroll_to_line_impl(src_line, window);
    }

    /// [`Self::scroll_to_line_impl`] 的精确落地（含宿主偏移写入），返回写入的目标
    /// **绝对偏移** `-(exact_top + pad_y)`；空文档/无宿主时返回 `f32::NAN`。编辑区⇄
    /// 预览同步的 `advance_pending_scroll` 复用同一落点，并以返回值逐帧核对是否稳住。
    fn scroll_to_line_impl(&mut self, src_line: usize, window: &mut Window) -> f32 {
        let n = self.doc.block_count();
        if n == 0 || self.doc.len() == 0 {
            return f32::NAN;
        }
        if src_line >= self.doc.line_count() {
            return f32::NAN;
        }
        let block = self.doc.block_index_for_line(src_line).min(n - 1);
        let width = self
            .rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        // 确保基础布局已建（未建则先建一次再读估计块顶；带外只取估算用于定位）。
        if self.rd.borrow().frame.borrow().is_none() {
            let band = self.shape_band().unwrap_or((0., 5000.));
            let f = element::build_frame(&*self, window, width, band);
            *self.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(f));
        }
        let est_top = self
            .rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.block_top(block))
            .unwrap_or(0.);
        let band = zoom_band_for_top(est_top, window.viewport_size().height);
        if self.shape_band() != Some(band) {
            self.set_shape_band(Some(band));
        }
        let frame = element::build_frame(&*self, window, width, band);
        let exact_top = frame.block_top(block);
        *self.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame));
        if let Some((vh, pad_y)) = self.host_scroll.borrow().clone() {
            let target = -(exact_top + pad_y);
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(target)));
            return target;
        }
        f32::NAN
    }

    /// 编辑区⇄预览滚动同步：把目标块**源码行**记入 `scroll_target`（宿主渲染期写入，
    /// 只存 Cell 不做任何布局/滚动）。WyEditor 元素 prepaint 经 [`Self::advance_pending_scroll`]
    /// 逐帧重钉宿主偏移直到稳住——提前在宿主 render 期调 `scroll_to_line` 会被本帧
    /// 布局复位，必须借 WY 自己的重钉循环（同缩放恢复收敛）。
    pub fn set_scroll_target(&self, src_line: usize) {
        self.scroll_target.set(Some(src_line));
        self.last_pin.set(None);
        self.scroll_applies.set(0);
    }

    /// 是否有待落地的滚动目标（prepaint 据此驱动重钉 + 刷帧）。
    pub(crate) fn scroll_target_active(&self) -> bool {
        self.scroll_target.get().is_some()
    }

    /// （测试探针）当前待落地的滚动目标源码行（P4-1 EPUB 翻章/目录跳转断言用）。
    pub fn scroll_target_line(&self) -> Option<usize> {
        self.scroll_target.get()
    }

    /// 编辑区⇄预览滚动同步的**重钉兜底**：最多逐帧施加这么多次后强制清除（防宿主
    /// 偏移被滚动条/布局持续复位的病态下永不收敛 → 每帧 `window.refresh()` + 重排
    /// 满载忙循环；对齐缩放恢复 [`Self::ZOOM_ANCHOR_MAX_APPLIES`]）。
    pub(crate) const SCROLL_TARGET_MAX_APPLIES: u32 = 30;

    /// 同步重钉在「复用态」下判定「用户已接管本窗滚动」的偏移阈值（px）：宿主当前偏移
    /// 偏离我方上次钉定目标超过该值 = 用户切过来滚了（一节滚轮 ≈ 54px+，绝无歧义）。
    /// 一旦判定即放弃重钉，避免与用户滚轮互搏（面板交接处上下跳动的直接根因）。
    pub(crate) const USER_TAKEOVER_PX: f32 = 8.0;
    /// 推进待落地滚动（WyEditor 元素 prepaint 每帧调用）：目标块**精确**顶经
    /// `scroll_to_line_impl` 写回宿主句柄。先读宿主当前偏移，若与上次写入的目标一致
    /// （说明布局未复位、已稳住）则清除目标停止重钉，否则重写并继续。
    ///
    /// - **后续帧复用 `last_pin` 缓存目标只写不重建**（首帧经 `scroll_to_line_impl`
    ///   已在目标带整形真值）：确认帧零成本，避免大文档上每帧 band 重建放大 CPU。
    /// - **[`Self::SCROLL_TARGET_MAX_APPLIES`] 兜底**：超限强制清除，绝不无限重钉。
    /// - 宿主未注入（live2 / 元素卸载期）：**丢弃残留目标**，宿主回归后按陈旧块号
    ///   重钉（仿缩放恢复宿主缺失即弃锚点）。
    pub(crate) fn advance_pending_scroll(&mut self, window: &mut Window) {
        let Some(sl) = self.scroll_target.get() else {
            return;
        };
        let applies = self.scroll_applies.get();
        if applies + 1 >= Self::SCROLL_TARGET_MAX_APPLIES {
            // 超限兜底：强制停止（最后一次 `scroll_to_line_impl` 已把精确目标写进宿主）。
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        }
        self.scroll_applies.set(applies + 1);
        let Some((vh, _pad_y)) = self.host_scroll.borrow().clone() else {
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        };
        let cur = (-vh.offset().y).as_f32();
        // 复用上次钉定目标（`last_pin` 与当前 `sl` 同源）则直接收敛，不再重建；
        // 首帧 / 目标块变化则经 `scroll_to_line_impl` 精确重建。
        let reused = match self.last_pin.get() {
            Some((psl, pt)) if psl == sl => Some(pt),
            _ => None,
        };
        let target = match reused {
            Some(pt) => pt,
            None => {
                // 首帧：精确重建远带帧 + 把目标写进宿主偏移。**重建出的远带帧先不装回显示帧**
                // ——容器 prepaint 已在本帧开头把「旧偏移」快照给本帧 paint，若本帧就画远帧，
                // 旧视口区域我会无行可画 → 一跳一白闪（EPUB 注记跳尾部注释实测 `visible=0`）。
                // 先把显示帧回退为跳转前的旧帧（本帧继续画旧页，无白闪），宿主偏移下帧生效后
                // 由 prepaint 按远带重建显示；上式复用帧的收敛判据仍会精确重钉到同一目标。
                let old = self.rd.borrow().frame.borrow().clone();
                let t = self.scroll_to_line_impl(sl, window);
                *self.rd.borrow_mut().frame.borrow_mut() = old;
                t
            }
        };
        if !target.is_finite() {
            self.scroll_target.set(None);
            self.last_pin.set(None);
            self.scroll_applies.set(0);
            return;
        }
        if reused.is_some() {
            // 复用态的收敛判定（不重建，避免大文档每帧 band 重建放大 CPU）：
            // `cur` = 宿主**内容偏移**（向下滚为正，`-vh.offset().y`）；`target` = 写入句柄的
            // **负**偏移（`scroll_to_line_impl` 返回 `-(exact_top + pad_y)` 并直接
            // `set_offset(target)`）。写入成功后二者互为相反数：cur == -target —— 收敛/接管
            // 判据因此按**同号**比较：`diff = cur + target` ≈ 0 = 已稳住；|diff| 超接管阈值 =
            // 用户滚走 / 偏移被布局复位（修：旧实现 `cur - target` 对远距跳转是「同向加倍」，
            // 永不 <1.5 → 每次都被误判成用户接管而提前放弃重钉）。
            let diff = cur + target;
            if diff.abs() < 1.5 {
                // 上一帧写入的目标在再次写入前仍被宿主持有 → 布局未复位，稳定落地。
                self.scroll_target.set(None);
                self.last_pin.set(None);
                self.scroll_applies.set(0);
                return;
            }
            if diff.abs() > Self::USER_TAKEOVER_PX {
                // 宿主已偏离目标超过「用户接管」阈值 = 用户在两边面板间切着滚（或其它
                // 控制流）抢走了本窗滚动 → 我方同步重钉立即放弃，**绝不与用户滚轮互搏**。
                // 这是「编辑区滚完切去预览区滚 / 反方向」上下跳动的直接根因：在途重钉
                // 每帧把用户刚滚的分量改写回目标。放弃后由 app 层 sync 正常接管（预览
                // 动了 → 驱动编辑器；两侧收敛由幂等回拉完成）。
                self.scroll_target.set(None);
                self.last_pin.set(None);
                self.scroll_applies.set(0);
                return;
            }
        }
        vh.set_offset(gpui::point(gpui::px(0.), gpui::px(target)));
        self.last_pin.set(Some((sl, target)));
    }
    /// 字号变化的**渲染期精确锚定**：在宿主 render（prepaint 读宿主滚动偏移**之前**）
    /// 按新字号重建窗口化帧，并把新布局里锚点块（缩放前视口顶部内容点）的精确位置
    /// 立即写回宿主 `ScrollHandle`——重排帧一开始就精确定位，不产生「旧偏移 × 新高度」
    /// 的中间错位帧（消除放大/缩小时的向上/向下跳动）。
    ///
    /// prepaint 的 stale 判定（rgen/igen/mgen/width/band）不含字号，帧已在此重建 →
    /// 本帧 prepaint 复用该帧不二次重建。锚点保留给 `advance_zoom_restore` 做带边缘/
    /// 细化的小幅收敛（带内块 build_frame 已整形真值，通常立即收敛）。
    pub fn prepare_font_zoom(&mut self, new_fs: Pixels, window: &mut Window) {
        if self.rd.borrow().theme.font_size == new_fs {
            return;
        }
        // 1) 用旧帧捕捉缩放锚点（宿主已推入 viewport_top）。
        self.capture_zoom_anchor();
        // 2) 记住当前宽/带，应用新字号并作废字号相关缓存（同 set_font_size）。
        let width = self
            .rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        let band = self.shape_band().unwrap_or((0., 5000.));
        self.rd.borrow_mut().theme.set_font_size(new_fs);
        *self.rd.borrow_mut().block_layout.borrow_mut() = None;
        self.rd.borrow_mut().shape_rows.borrow_mut().clear();
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear();
        // 3) 渲染期重建窗口化帧 → 拿到新布局的精确块 top/height。
        let frame = element::build_frame(&*self, window, width, band);
        // 4) 锚点块在新布局里的精确位置 = 恢复目标（元素局部 y）。
        let target = {
            let anchor = *self.zoom_anchor.borrow();
            match anchor {
                Some((block, frac)) => {
                    let count = frame.block_count();
                    let block = block.min(count.saturating_sub(1));
                    let h = frame.block_height(block).max(1.);
                    frame.block_top(block) + frac * h
                }
                None => 0.0,
            }
        };
        // 5) 写回宿主滚动句柄，并按「锚定后的内容顶」推导新可视带（与 element prepaint
        //    的 band 计算同式）——若带跨界，直接以新带再建一帧并发布给 `shape_band`，
        //    让本帧 measure/prepaint 复用的都是同一帧：既避免「prepare 用旧带 / prepaint
        //    用新带」的冗余重建，也消除两者之间 `shape_band` 的瞬态不一致。
        let host = self.host_scroll.borrow().clone();
        let final_band = if let Some((vh, pad_y)) = host {
            vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
            zoom_band_for_top(target + pad_y, window.viewport_size().height)
        } else {
            band
        };
        let frame = if final_band != band {
            self.set_shape_band(Some(final_band));
            element::build_frame(&*self, window, width, final_band)
        } else {
            frame
        };
        *self.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame));
    }
    /// 宿主每帧推入当前视口顶部 px（元素局部 y，>=0）。缩放锚点 `capture_zoom_anchor`
    /// 以此为准；宿主用 `#live-scroll` 的 `ScrollHandle::offset()` 减去 #live-body 顶部
    /// 内衬（见 app::panes）得到。
    pub fn set_viewport_top(&self, top_px: f32) {
        self.viewport_top_px.set(top_px.max(0.));
    }

    /// 第 `top_px`（元素局部 y）落在的块 + 块内比例：`(块下标, (top − 块顶)/块高)`。
    /// 供缩放锚点捕捉与（测试）断言缩放后视口顶部内容点不变。
    pub fn block_of_px(&self, top_px: f32) -> Option<(usize, f32)> {
        let f = self.rd.borrow().frame.borrow().clone()?;
        let n = f.block_count();
        if n == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if f.block_top(mid) <= top_px {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let block = lo.saturating_sub(1).min(n - 1);
        let h = f.block_height(block).max(1.);
        Some((block, ((top_px - f.block_top(block)) / h).clamp(0.0, 1.0)))
    }

    /// 当前视口顶部落点块的**起始源码行**（0 基；帧未就绪返回 `None`）。编辑区⇄
    /// 预览滚动同步用——预览侧同步键 = 该行所在块的起点，与 [`Self::block_start_of_line`]
    /// 同口径（都以块对齐：块内微滚动不拉动对端）。
    pub fn top_source_line(&self) -> Option<usize> {
        let f = self.rd.borrow().frame.borrow().clone()?;
        let (block, _) = self.block_of_px(self.viewport_top_px.get())?;
        Some(f.base.borrow().src_line(block) as usize)
    }

    /// 源码行 → 所在块的**起始源码行**（0 基；块按 `src_line` 升序连续覆盖，越界归
    /// 首/末块；0 块时原样返回）。滚动同步把「编辑器顶行」归一成块起点，与预览顶
    /// 块比较——同一段落/代码块内微滚动双方键不变，不互相拉动。
    pub fn block_start_of_line(&self, line: usize) -> usize {
        let n = self.doc.block_count();
        if n == 0 {
            return line;
        }
        let ix = self.doc.block_index_for_line(line).min(n.saturating_sub(1));
        self.doc
            .block_meta(ix)
            .map(|m| m.src_line as usize)
            .unwrap_or(line)
    }
    /// 重排前捕捉缩放锚点：当前视口顶部 px → 落在的块 + 块内比例。
    pub(crate) fn capture_zoom_anchor(&self) {
        if let Some(a) = self.block_of_px(self.viewport_top_px.get()) {
            *self.zoom_anchor.borrow_mut() = Some(a);
        }
    }

    /// 宿主把 WYSIWYG 宿主滚动容器（`ScrollHandle`）与内容顶部内衬注入；`None` =
    /// 非宿主（独立测试/未进入 live）时缩放恢复不驱动滚轮。句柄与内衬已被当前值覆盖
    /// 时直接早退——宿主每帧都调，避免反复 RefCell 写 + Rc 克隆。
    pub fn set_host_scroll(&mut self, handle: Option<gpui::ScrollHandle>, pad_y: f32) {
        if handle.is_some()
            && self
                .host_scroll
                .borrow()
                .as_ref()
                .is_some_and(|(_, p)| *p == pad_y)
        {
            return;
        }
        *self.host_scroll.borrow_mut() = handle.map(|h| (h, pad_y));
    }

    /// 当前宿主滚动句柄（供元素 prepaint 写回缩放恢复目标）。
    pub fn host_scroll(&self) -> Option<(gpui::ScrollHandle, f32)> {
        self.host_scroll.borrow().clone()
    }
    /// 推进（并最终消费）缩放恢复：返回 `Some((目标局部 y, done))`，`done` = 是否已收敛
    /// 消费锚点。**由 WyEditor 自身在 element prepaint 每帧驱动**（宿主应用只负责注入
    /// 句柄 + 推视口顶 + 改字号）——因为字号变化后 frame 逐帧把「估计高」细化为「整形
    /// 真值」，`block_top/height` 缓慢变动；必须在本编辑器自己的重排帧里逐帧重钉，直到
    /// 目标位移 < [`Self::ZOOM_ANCHOR_EPS`] 或超过 [`Self::ZOOM_ANCHOR_MAX_APPLIES`] 帧
    /// 才停（防极端文档高度持续细化时锚点永不收敛、永远覆盖用户滚动）。
    ///
    /// 帧未就绪返回 `None`（下帧再取）；未注入宿主句柄时直接丢弃锚点（不进死循环）。
    pub(crate) fn advance_zoom_restore(&self) -> Option<(f32, bool)> {
        // 先查锚点（绝大多数帧为 None，最早退出）；再查宿主句柄，缺则丢弃锚点（不进死循环）。
        let anchor = (*self.zoom_anchor.borrow())?;
        if self.host_scroll.borrow().is_none() {
            *self.zoom_anchor.borrow_mut() = None;
            self.zoom_last_target.set(None);
            self.zoom_applies.set(0);
            return None;
        }
        let f = self.rd.borrow().frame.borrow().clone()?;
        let (block, frac) = anchor;
        let count = f.block_count();
        let block = block.min(count.saturating_sub(1));
        let h = f.block_height(block).max(1.);
        let target = f.block_top(block) + frac * h;
        let applies = self.zoom_applies.get();
        let done = if let Some(prev) = self.zoom_last_target.get() {
            (target - prev).abs() < Self::ZOOM_ANCHOR_EPS
                || applies + 1 >= Self::ZOOM_ANCHOR_MAX_APPLIES
        } else {
            false
        };
        self.zoom_last_target.set(Some(target));
        self.zoom_applies.set(applies + 1);
        if done {
            *self.zoom_anchor.borrow_mut() = None;
            self.zoom_last_target.set(None);
            self.zoom_applies.set(0);
        }
        Some((target, done))
    }
    /// 代码块横向滚动偏移（键 = 代码块首源码行）。视图本地状态（不写源码）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    /// P3 / Slice C：读统一走 `HScrollState::scroll_of`。
    #[allow(dead_code)]
    pub(crate) fn code_h_scroll_of(&self, src_line: usize) -> f32 {
        self.rd.borrow().h_scroll.scroll_of(HScrollKind::Code, src_line)
    }
    /// 设置代码块横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    /// 仅供宿主测试消费的旧签名转发（生产经 `HScrollState::set_scroll` 写）→ allow 压
    /// dead_code 告警。P3 / Slice C：写统一走 `HScrollState::set_scroll` 单一实现。
    #[allow(dead_code)]
    pub(crate) fn set_code_h_scroll(&self, src_line: usize, v: f32) {
        self.rd.borrow().h_scroll.set_scroll(HScrollKind::Code, src_line, v);
    }

    /// 表格横向滚动偏移（px；键 = 表格块首源码行）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    /// P3 / Slice C：读统一走 `HScrollState::scroll_of`。
    #[allow(dead_code)]
    pub(crate) fn table_h_scroll_of(&self, src_line: usize) -> f32 {
        self.rd.borrow().h_scroll.scroll_of(HScrollKind::Table, src_line)
    }
    /// 设置表格横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    /// 仅供宿主测试消费的旧签名转发（生产经 `HScrollState::set_scroll` 写）→ allow 压
    /// dead_code 告警。P3 / Slice C：写统一走 `HScrollState::set_scroll` 单一实现。
    #[allow(dead_code)]
    pub(crate) fn set_table_h_scroll(&self, src_line: usize, v: f32) {
        self.rd.borrow().h_scroll.set_scroll(HScrollKind::Table, src_line, v);
    }

    /// 数学卡横向滚动偏移（px；键 = 数学块首源码行）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    /// P3 / Slice C：读统一走 `HScrollState::scroll_of`。
    #[allow(dead_code)]
    pub(crate) fn math_h_scroll_of(&self, src_line: usize) -> f32 {
        self.rd.borrow().h_scroll.scroll_of(HScrollKind::Math, src_line)
    }
    /// 设置数学卡横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    /// 仅供宿主测试消费的旧签名转发（生产经 `HScrollState::set_scroll` 写）→ allow 压
    /// dead_code 告警。P3 / Slice C：写统一走 `HScrollState::set_scroll` 单一实现。
    #[allow(dead_code)]
    pub(crate) fn set_math_h_scroll(&self, src_line: usize, v: f32) {
        self.rd.borrow().h_scroll.set_scroll(HScrollKind::Math, src_line, v);
    }
    /// 数学卡内容**自然宽**侧缓存（px；键 = 数学块首源码行）——paint 每次画卡时更新
    /// （引擎侧实现，宿主转发）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn set_math_natural_w(&self, src_line: usize, w: f32) {
        markdown_wysiwyg_engine::frame::set_math_natural_w(&self.rd, src_line, w);
    }
    /// 数学卡内容**自然宽**（px；键 = 数学块首源码行）：paint 侧缓存，未画过 = 0。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn math_natural_w_of(&self, src_line: usize) -> f32 {
        self.rd
            .borrow()
            .math_natural_w
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(0.)
    }

    /// 结构编辑后清除「以 src_line 为键」的块视图态（代码/表格/数学横向滚动、
    /// mermaid 缩放、数学自然宽侧缓存）：被结构替换**行窗**（`edit_line ..=
    /// edit_line + max(删/插换行)`）内的旧键会误套到同线新块（把旧块的横向滚动
    /// /缩放带到新块），故按窗清；**窗外的块内容未变**（仅行号平移，旧键落空
    /// 自愈）→ 保留，勿误清用户有意保留的滚动。
    /// `math_pins`/`notdef_retry` 已在编辑管道同期清理（见 `invalidate_after_edit`）；
    /// `folded_headings` 按**文本键**存、不随行号错位（改标题文本键自然失效，折叠体
    /// 由 derived 平移/重建）→ 无需清理（结构编辑失效机理对照）。
    pub(crate) fn clear_src_line_view_state(&self, edit_line: usize, max_nl: usize) {
        // P3 / Slice C：三段横向滚动（code/table/math）窗口 retain 收进
        // `HScrollState::retain_src_line_window`（窗口语义与次序不变）；`math_natural_w` /
        // `mermaid_zoom` 的 retain 保留在宿主侧。
        self.rd
            .borrow()
            .h_scroll
            .retain_src_line_window(edit_line, max_nl);
        let end = edit_line.saturating_add(max_nl);
        self.rd
            .borrow_mut()
            .math_natural_w
            .borrow_mut()
            .retain(|&l, _| l < edit_line || l > end);
        self.rd
            .borrow_mut()
            .mermaid_zoom
            .borrow_mut()
            .retain(|&l, _| l < edit_line || l > end);
    }
    /// 记录本帧元素在窗口中的 bounds（paint 调用；鼠标命中换算用）。
    pub(crate) fn set_last_bounds(&self, bounds: Option<gpui::Bounds<Pixels>>) {
        self.last_bounds.set(bounds);
    }
    pub fn last_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
        self.last_bounds.get()
    }

    /// 居中阅读栏的**水平移位**（px）：`(元素可用宽 − 帧布局宽) ÷ 2`。
    ///
    /// 帧宽被 measure 钳到 ≤ 72ch（`READING_COL_ENABLED`），元素仍占满面板——内容列整列
    /// （含 gutter/负缩进/引述条）需在面板内水平居中。所有把窗口坐标换算回**内容区内部**
    /// 坐标系（`to_local`/`source_offset_at`/横向滚动条拖拽）都必须叠减该偏移；paint/hitbox
    /// 把元素屏幕原点叠加该偏移。内容区内部的几何（`padding`/`x_off`/光标/选区）一律不变，
    /// 拖拽/点击/光标对齐因此天然保持。未启用或窗口窄于列宽时 = 0（不破坏原语义）。
    pub(crate) fn reading_col_shift(&self, elem_w: f32) -> f32 {
        markdown_wysiwyg_engine::frame::reading_col_shift(&self.rd, elem_w)
    }

    /// 内容列在**窗口坐标**中的屏幕原点（元素屏幕原点 + 阅读栏水平移位）。
    /// open（非 paint 期）需要与帧同代摆卡片/命中时用（paint 期用传入的 `bounds.origin`
    /// 现算，这里供 `image_edit_anchor` 等静态换算路径复用同一语义）。
    pub(crate) fn content_origin(&self) -> gpui::Point<Pixels> {
        let (origin, elem_w) = match self.last_bounds() {
            Some(b) => (b.origin, f32::from(b.size.width)),
            None => (gpui::point(px(0.), px(0.)), 0.),
        };
        let shift = self.reading_col_shift(elem_w);
        gpui::point(origin.x + px(shift), origin.y)
    }
    /// 内容结构版本：任何内容变更（编辑/换源/整篇安装）递增，O(1)。
    /// 侧边栏 TOC 缓存的失效依据（与条目缓存 `toc_cache` 同键，见 [`WyEditor::toc_entries`]）。
    pub fn content_gen(&self) -> u64 {
        self.rd.borrow().content_gen.get()
    }
    pub(crate) fn set_shape_band(&self, band: Option<(f32, f32)>) {
        self.rd.borrow_mut().shape_band.set(band);
    }
    pub(crate) fn shape_band(&self) -> Option<(f32, f32)> {
        self.rd.borrow().shape_band.get()
    }
    /// 内容结构版本（基础布局代组成；结构性编辑才递增）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn struct_ver(&self) -> u64 {
        self.rd.borrow().struct_ver.get()
    }
    /// 结构编辑影响的首个源行（Pass1 增量前缀复用边界）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn struct_dirty_line(&self) -> usize {
        self.rd.borrow().struct_dirty_line.get()
    }
    /// 编辑影响的最后一个旧源行（Pass1 定位旧尾部起点用）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn old_edit_end_line(&self) -> usize {
        self.rd.borrow().struct_old_end_line.get()
    }
    /// 编辑的源行净增（插入换行 − 被删换行）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn edit_line_delta(&self) -> i64 {
        self.rd.borrow().struct_line_delta.get()
    }
    /// 派生版本（`derived()` 缓存键；供回归断言「纯文本键入不重建派生」）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn regions_ver(&self) -> u64 {
        self.rd.borrow().regions_ver.get()
    }
    /// 结构编辑「行号平移」版本（`fp_sets` 缓存键组成部分；`shift_derived_after_edit`
    /// 成功就地平移区域行号时递增——region 结构未重建、只是行号动了）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn regions_shift_ver(&self) -> u64 {
        self.rd.borrow().regions_shift_ver.get()
    }
    /// 标题折叠视图态版本（`fold_plan` 缓存键组成部分）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn fold_ver(&self) -> u64 {
        self.rd.borrow().fold_ver.get()
    }
    /// 当前缓存的折叠计划 Rc（滚动预热读取同一份带无关缓存；测试断言「仅移动光标/
    /// 纯文本键入不重建折叠计划」也用它）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn fold_plan_rc(&self) -> Option<Rc<element::FoldPlan>> {
        self.rd.borrow().fold_plan.borrow().as_ref().map(|(_, _, p)| p.clone())
    }

    /// 取走待修补（重估高度）的块下标（编辑后一帧用；无则 None）。
    /// 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn take_pending_patch_blocks(&self) -> Option<Vec<usize>> {
        self.rd.borrow_mut().pending_patch_blocks.borrow_mut().take()
    }

    /// （仅测试）自上次读取以来 Pass2 实际整形次数（缓存未命中 / 光标行），
    /// 供回归断言「滚动/同带重建复用缓存、编辑只重排受影响块」。
    #[cfg(test)]
    pub(crate) fn take_reshapes(&self) -> u64 {
        let rd = self.rd.borrow();
        let v = rd.reshapes.get();
        rd.reshapes.set(0);
        v
    }
    /// （仅测试）行整形缓存当前条目数。
    #[cfg(test)]
    pub(crate) fn shape_rows_len(&self) -> usize {
        self.rd.borrow().shape_rows.borrow().len()
    }
    /// （仅测试）非结构性编辑后：被编辑块（内容变了 → 指纹变）应从缓存剔除。
    #[cfg(test)]
    pub(crate) fn shape_rows_has(&self, block: usize) -> bool {
        let cid = frame::block_content_id(&self.doc, block);
        self.rd.borrow().shape_rows.borrow().entries.contains_key(&cid)
    }
    /// （仅测试）当前缓存的基础布局代键（`height_epoch` 结果）——M3 A4 观测点：
    /// 图片占位→真实 / 拖拽改宽走**稀疏覆写**（不进代），代不变；结构性编辑/换宽才换代。
    #[cfg(test)]
    pub(crate) fn block_layout_epoch(&self) -> Option<u64> {
        self.rd.borrow().block_layout.borrow().as_ref().map(|(e, _)| *e)
    }
    /// （仅测试）第 `i` 块的基础布局高（不含动态增量）——断言稀疏覆写后的带外/带内
    /// 图片块高（Frame::block_height 只对完整帧，这里读编辑器基表本体）。
    #[cfg(test)]
    pub(crate) fn block_base_height(&self, i: usize) -> Option<f32> {
        self.rd.borrow().block_layout.borrow().as_ref().map(|(_, b)| b.borrow().height(i))
    }
    /// （仅测试）Pass2 光栅计数（缓存未命中/光标行时自增）。
    /// 当前无测试调用（E4 遗留）→ allow 压 dead_code 告警。
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn bump_reshapes(&self) {
        let rd = self.rd.borrow();
        rd.reshapes.set(rd.reshapes.get() + 1);
    }
    /// （仅测试）Pass2 notdef 冷却帧计数（复用旧几何、未调用 gpui shape）。
    /// 当前无测试调用（E4 遗留）→ allow 压 dead_code 告警。
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn bump_coolings(&self) {
        let rd = self.rd.borrow();
        rd.coolings.set(rd.coolings.get() + 1);
    }
    /// （仅测试）D1 预热整形计数（`prewarm_band` 每实际 shape 一块自增）。
    /// 当前无测试调用（E4 遗留）→ allow 压 dead_code 告警。
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn bump_prewarm_shapes(&self) {
        let rd = self.rd.borrow();
        rd.prewarm_shapes.set(rd.prewarm_shapes.get() + 1);
    }
    /// （仅测试）自上次读取以来 D1 预热实际整形次数。
    #[cfg(test)]
    pub(crate) fn take_prewarm_shapes(&self) -> u64 {
        let rd = self.rd.borrow();
        let v = rd.prewarm_shapes.get();
        rd.prewarm_shapes.set(0);
        v
    }
    /// 折叠/重排计划（缓存）已随 `fold_plan`/`fold_reveal` 等迁入引擎 `frame` 模块：
    /// 宿主侧不再持有实现，按需经 `markdown_wysiwyg_engine::frame::{fold_plan,fold_reveal}`。
    pub(crate) fn derived(&self) -> Rc<RefCell<WyDerived>> {
        markdown_wysiwyg_engine::frame::derived(&self.rd, &self.doc)
    }

    // —— notdef 自愈 / 基础布局编排：算法体已随引擎迁 `markdown_wysiwyg_engine::frame`，
    // —— 以下保留宿主签名转发，测试（`element/tests/{notdef,note_def_flip,prebuild_pass1}.rs`）
    // —— 与宿主编排路径对 `self.X()` 的引用原样成立。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn ensure_block_layout(
        &self,
        n_blocks: usize,
        epoch: u64,
    ) -> (bool, BlockLayoutRef, Option<BlockLayoutRef>) {
        markdown_wysiwyg_engine::frame::ensure_block_layout(&self.rd, n_blocks, epoch)
    }
    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn cooling_row(&self, block: usize) -> Option<Rc<element::ShapedRow>> {
        markdown_wysiwyg_engine::frame::cooling_row(&self.rd, block)
    }

    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn cool_start(&self, block: usize) -> bool {
        markdown_wysiwyg_engine::frame::cool_start(&self.rd, block)
    }

    // 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn cool_keep_last(&self, block: usize, row: Rc<element::ShapedRow>) {
        markdown_wysiwyg_engine::frame::cool_keep_last(&self.rd, block, row);
    }

    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn notdef_settled(&self, block: usize) -> bool {
        markdown_wysiwyg_engine::frame::notdef_settled(&self.rd, block)
    }

    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn clear_notdef(&self, block: usize) {
        markdown_wysiwyg_engine::frame::clear_notdef(&self.rd, block);
    }

    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn fallback_verified(&self, block: usize) -> bool {
        markdown_wysiwyg_engine::frame::fallback_verified(&self.rd, block)
    }

    // 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn mark_fallback_verified(&self, block: usize) {
        markdown_wysiwyg_engine::frame::mark_fallback_verified(&self.rd, block);
    }

    // 预留旧签名转发（生产/测试均无消费；dormancy 保留）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn verify_cool(&self, block: usize) {
        markdown_wysiwyg_engine::frame::verify_cool(&self.rd, block);
    }

    /// 换代自愈（仅测试；生产 Pass2 现以内联 `notdef_retry.clear_stale` 经 `rd` 调用）。
    #[cfg(test)]
    pub(crate) fn clear_stale_verdicts(&self, block: usize) {
        self.rd.borrow_mut().notdef_retry.borrow_mut().clear_stale(block);
    }
}
