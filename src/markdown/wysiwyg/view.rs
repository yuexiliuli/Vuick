use super::*;

use crate::markdown::parse::inline_style;

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
    pub(crate) fn scroll_to_line(&mut self, src_line: usize, window: &mut Window) {
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
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        // 确保基础布局已建（未建则先建一次再读估计块顶；带外只取估算用于定位）。
        if self.frame.borrow().is_none() {
            let band = self.shape_band().unwrap_or((0., 5000.));
            let f = element::build_frame(&*self, window, width, band);
            *self.frame.borrow_mut() = Some(std::rc::Rc::new(f));
        }
        let est_top = self
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
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
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
    pub(crate) fn set_scroll_target(&self, src_line: usize) {
        self.scroll_target.set(Some(src_line));
        self.last_pin.set(None);
        self.scroll_applies.set(0);
    }

    /// 是否有待落地的滚动目标（prepaint 据此驱动重钉 + 刷帧）。
    pub(crate) fn scroll_target_active(&self) -> bool {
        self.scroll_target.get().is_some()
    }

    /// （仅测试）当前待落地的滚动目标源码行（P4-1 EPUB 翻章/目录跳转断言用）。
    #[cfg(test)]
    pub(crate) fn scroll_target_line(&self) -> Option<usize> {
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
                let old = self.frame.borrow().clone();
                let t = self.scroll_to_line_impl(sl, window);
                *self.frame.borrow_mut() = old;
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
    pub(crate) fn prepare_font_zoom(&mut self, new_fs: Pixels, window: &mut Window) {
        if self.theme.font_size == new_fs {
            return;
        }
        // 1) 用旧帧捕捉缩放锚点（宿主已推入 viewport_top）。
        self.capture_zoom_anchor();
        // 2) 记住当前宽/带，应用新字号并作废字号相关缓存（同 set_font_size）。
        let width = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f.width)
            .unwrap_or(px(800.));
        let band = self.shape_band().unwrap_or((0., 5000.));
        self.theme.set_font_size(new_fs);
        *self.block_layout.borrow_mut() = None;
        self.shape_rows.borrow_mut().clear();
        self.notdef_retry.borrow_mut().clear();
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
        *self.frame.borrow_mut() = Some(std::rc::Rc::new(frame));
    }
    /// 宿主每帧推入当前视口顶部 px（元素局部 y，>=0）。缩放锚点 `capture_zoom_anchor`
    /// 以此为准；宿主用 `#live-scroll` 的 `ScrollHandle::offset()` 减去 #live-body 顶部
    /// 内衬（见 app::panes）得到。
    pub(crate) fn set_viewport_top(&self, top_px: f32) {
        self.viewport_top_px.set(top_px.max(0.));
    }

    /// 第 `top_px`（元素局部 y）落在的块 + 块内比例：`(块下标, (top − 块顶)/块高)`。
    /// 供缩放锚点捕捉与（测试）断言缩放后视口顶部内容点不变。
    pub(crate) fn block_of_px(&self, top_px: f32) -> Option<(usize, f32)> {
        let f = self.frame.borrow().clone()?;
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
    pub(crate) fn top_source_line(&self) -> Option<usize> {
        let f = self.frame.borrow().clone()?;
        let (block, _) = self.block_of_px(self.viewport_top_px.get())?;
        Some(f.base.borrow().src_line(block) as usize)
    }

    /// 源码行 → 所在块的**起始源码行**（0 基；块按 `src_line` 升序连续覆盖，越界归
    /// 首/末块；0 块时原样返回）。滚动同步把「编辑器顶行」归一成块起点，与预览顶
    /// 块比较——同一段落/代码块内微滚动双方键不变，不互相拉动。
    pub(crate) fn block_start_of_line(&self, line: usize) -> usize {
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
    pub(crate) fn set_host_scroll(&mut self, handle: Option<gpui::ScrollHandle>, pad_y: f32) {
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
    pub(crate) fn host_scroll(&self) -> Option<(gpui::ScrollHandle, f32)> {
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
        let f = self.frame.borrow().clone()?;
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
    // —— `.notdef`（回退未命中）坏行自愈 ——
    // Windows 冷启动时 DWrite 对主字体缺的字形（❗⚠✅ 等，需走回退字体）首次解析可能
    // 直接给出 .notdef（glyph id=0，空心矩形）。若这个坏 `ShapedRow` 被行缓存钉住，
    // 整次运行都不消失。实测确认的根因（115c131 失败点）：gpui 的 `LineLayoutCache`
    // 把平台 `layout_line` 结果按 `(text, font, runs)` 键缓存，**与 wrap-width 无关**——
    // 只扰动宽度永远打不到 DWrite，重试 N 帧 → 给出 → 豆腐块钉住。这里改为：命中/入
    // 缓存前扫 .notdef → 坏行不入缓存，进入「冷却一帧 → 新鲜重排」循环（冷却帧不调用
    // gpui shape，让被钉的键自然过期），每次重排都真正打到平台，直到回退解析成功；
    // 连续 [`NOTDEF_RETRY_ATTEMPTS`] 次仍坏才判定字符确无字形（tofu 即正确显示）。
    //
    // **不要**把「回退已温」当作「.notdef = 确无字形」的短路：探针只覆盖少数 (font,size)
    // 组合，真实行的字号/字体可能未探到而首排仍 .notdef（实测首排坏、次排好）——所以
    // 一律走重试，由同位（font,size）的若干次真实重排自行恢复，绝不静默缓存成豆腐块。

    /// 冷却帧取上一份 notdef 行几何（不调用 gpui shape → 被钉的 LineLayoutCache 键
    /// 本帧不被引用、帧末自然淘汰）。返回 `Some` 表示本块处于冷却帧、应直接复用返回行。
    pub(crate) fn cooling_row(&self, block: usize) -> Option<std::rc::Rc<element::ShapedRow>> {
        let mut m = self.notdef_retry.borrow_mut();
        if !m.cooling.remove(&block) {
            return None;
        }
        m.last.remove(&block)
    }

    /// 登记一次 notdef（推进真实平台重排计数 + 置冷却）。返回 `true` 表示已达
    /// [`NOTDEF_RETRY_ATTEMPTS`] 上限 → 判定字符确无字形（settled），本次结果应入缓存。
    pub(crate) fn cool_start(&self, block: usize) -> bool {
        let mut m = self.notdef_retry.borrow_mut();
        let cnt = m.attempts.get(&block).copied().unwrap_or(0) + 1;
        if cnt >= NOTDEF_RETRY_ATTEMPTS {
            m.attempts.remove(&block);
            m.cooling.remove(&block);
            m.last.remove(&block);
            m.settled.insert(block);
            true
        } else {
            m.attempts.insert(block, cnt);
            m.cooling.insert(block);
            false
        }
    }

    /// notdef 后保存本帧几何（供随后的冷却帧 [`Self::cooling_row`] 显示复用）。
    /// **勿删**：冷却帧不调用 gpui shape，若丢弃上一份几何会让该行在冷却帧「没有可画
    /// 的行」或退化为空占位 → 几何与新鲜重排不一致 → 冷却帧闪跳（换代/滚动时尤其显眼）。
    /// 保留它 = 以「可能仍坏的上一帧」换「不闪跳」，与 `NotdefRetry::last` 字段配套。
    pub(crate) fn cool_keep_last(&self, block: usize, row: std::rc::Rc<element::ShapedRow>) {
        self.notdef_retry.borrow_mut().last.insert(block, row);
    }

    /// 该块是否已判定「确无字形」（settled）：是 → 命中含 notdef 的缓存不再当 miss。
    pub(crate) fn notdef_settled(&self, block: usize) -> bool {
        self.notdef_retry.borrow().settled.contains(&block)
    }

    pub(crate) fn clear_notdef(&self, block: usize) {
        self.notdef_retry.borrow_mut().clear_block(block);
    }

    /// 换代自愈：块被新代键重排前清掉上一代判决（attempts/settled/verified/last），
    /// 保留冷却。由 Pass2 在检测到 `RowShapeCache` 键换代时调用（见 build_frame）。
    pub(crate) fn clear_stale_verdicts(&self, block: usize) {
        self.notdef_retry.borrow_mut().clear_stale(block);
    }

    /// 该块是否已做过「回退字形校验重排」（见 [`NotdefRetry::verified`]）。
    pub(crate) fn fallback_verified(&self, block: usize) -> bool {
        self.notdef_retry.borrow().verified.contains(&block)
    }

    pub(crate) fn mark_fallback_verified(&self, block: usize) {
        self.notdef_retry.borrow_mut().verified.insert(block);
    }

    /// 回退校验冷却：只置冷却标志（不占 notdef 重试次数）——下一帧不 shape、让被钉的
    /// LineLayoutCache 键淘汰，再下一帧带新键真打到平台拿到稳定回退字形。
    pub(crate) fn verify_cool(&self, block: usize) {
        self.notdef_retry.borrow_mut().cooling.insert(block);
    }
    /// 代码块横向滚动偏移（键 = 代码块首源码行）。视图本地状态（不写源码）。
    pub(crate) fn code_h_scroll_of(&self, src_line: usize) -> f32 {
        self.code_h_scroll
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(0.)
    }
    /// 设置代码块横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    pub(crate) fn set_code_h_scroll(&self, src_line: usize, v: f32) {
        let v = v.max(0.);
        if v <= 0.01 {
            self.code_h_scroll.borrow_mut().remove(&src_line);
        } else {
            self.code_h_scroll.borrow_mut().insert(src_line, v);
        }
    }

    /// 表格横向滚动偏移（px；键 = 表格块首源码行）。
    pub(crate) fn table_h_scroll_of(&self, src_line: usize) -> f32 {
        self.table_h_scroll
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(0.)
    }
    /// 设置表格横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    pub(crate) fn set_table_h_scroll(&self, src_line: usize, v: f32) {
        let v = v.max(0.);
        if v <= 0.01 {
            self.table_h_scroll.borrow_mut().remove(&src_line);
        } else {
            self.table_h_scroll.borrow_mut().insert(src_line, v);
        }
    }

    /// 数学卡横向滚动偏移（px；键 = 数学块首源码行）。
    pub(crate) fn math_h_scroll_of(&self, src_line: usize) -> f32 {
        self.math_h_scroll
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(0.)
    }
    /// 设置数学卡横向滚动偏移（px；实际交由 paint 侧按内容宽 clamp）。调用方 notify。
    pub(crate) fn set_math_h_scroll(&self, src_line: usize, v: f32) {
        let v = v.max(0.);
        if v <= 0.01 {
            self.math_h_scroll.borrow_mut().remove(&src_line);
        } else {
            self.math_h_scroll.borrow_mut().insert(src_line, v);
        }
    }
    /// 数学卡内容**自然宽**侧缓存（px；键 = 数学块首源码行）——paint 每次画卡时更新。
    pub(crate) fn set_math_natural_w(&self, src_line: usize, w: f32) {
        if w > 0.01 {
            self.math_natural_w.borrow_mut().insert(src_line, w);
        } else {
            self.math_natural_w.borrow_mut().remove(&src_line);
        }
    }
    /// 数学卡内容**自然宽**（px；键 = 数学块首源码行）：paint 侧缓存，未画过 = 0。
    pub(crate) fn math_natural_w_of(&self, src_line: usize) -> f32 {
        self.math_natural_w
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(0.)
    }
    /// 记录本帧元素在窗口中的 bounds（paint 调用；鼠标命中换算用）。
    pub(crate) fn set_last_bounds(&self, bounds: Option<gpui::Bounds<Pixels>>) {
        self.last_bounds.set(bounds);
    }
    pub(crate) fn last_bounds(&self) -> Option<gpui::Bounds<Pixels>> {
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
        let fw = self
            .frame
            .borrow()
            .as_ref()
            .map(|f| f32::from(f.width))
            .unwrap_or(elem_w);
        ((elem_w - fw) / 2.).max(0.)
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
    pub(crate) fn content_gen(&self) -> u64 {
        self.content_gen.get()
    }
    pub(crate) fn set_shape_band(&self, band: Option<(f32, f32)>) {
        self.shape_band.set(band);
    }
    pub(crate) fn shape_band(&self) -> Option<(f32, f32)> {
        self.shape_band.get()
    }
    /// 内容结构版本（基础布局代组成；结构性编辑才递增）。
    pub(crate) fn struct_ver(&self) -> u64 {
        self.struct_ver.get()
    }
    /// 结构编辑影响的首个源行（Pass1 增量前缀复用边界）。
    pub(crate) fn struct_dirty_line(&self) -> usize {
        self.struct_dirty_line.get()
    }
    /// 编辑影响的最后一个旧源行（Pass1 定位旧尾部起点用）。
    pub(crate) fn old_edit_end_line(&self) -> usize {
        self.struct_old_end_line.get()
    }
    /// 编辑的源行净增（插入换行 − 被删换行）。
    pub(crate) fn edit_line_delta(&self) -> i64 {
        self.struct_line_delta.get()
    }
    /// 派生版本（`derived()` 缓存键；供回归断言「纯文本键入不重建派生」）。
    pub(crate) fn regions_ver(&self) -> u64 {
        self.regions_ver.get()
    }
    /// 结构编辑「行号平移」版本（`fp_sets` 缓存键组成部分；`shift_derived_after_edit`
    /// 成功就地平移区域行号时递增——region 结构未重建、只是行号动了）。
    pub(crate) fn regions_shift_ver(&self) -> u64 {
        self.regions_shift_ver.get()
    }
    /// 标题折叠视图态版本（`fold_plan` 缓存键组成部分）。
    pub(crate) fn fold_ver(&self) -> u64 {
        self.fold_ver.get()
    }
    /// 当前缓存的折叠计划 Rc（滚动预热读取同一份带无关缓存；测试断言「仅移动光标/
    /// 纯文本键入不重建折叠计划」也用它）。
    pub(crate) fn fold_plan_rc(&self) -> Option<Rc<element::FoldPlan>> {
        self.fold_plan.borrow().as_ref().map(|(_, _, p)| p.clone())
    }

    /// 取走待修补（重估高度）的块下标（编辑后一帧用；无则 None）。
    pub(crate) fn take_pending_patch_blocks(&self) -> Option<Vec<usize>> {
        self.pending_patch_blocks.borrow_mut().take()
    }

    /// （仅测试）自上次读取以来 Pass2 实际整形次数（缓存未命中 / 光标行），
    /// 供回归断言「滚动/同带重建复用缓存、编辑只重排受影响块」。
    #[cfg(test)]
    pub(crate) fn take_reshapes(&self) -> u64 {
        let v = self.reshapes.get();
        self.reshapes.set(0);
        v
    }
    /// （仅测试）行整形缓存当前条目数。
    #[cfg(test)]
    pub(crate) fn shape_rows_len(&self) -> usize {
        self.shape_rows.borrow().len()
    }
    /// （仅测试）非结构性编辑后：被编辑块（内容变了 → 指纹变）应从缓存剔除。
    #[cfg(test)]
    pub(crate) fn shape_rows_has(&self, block: usize) -> bool {
        let cid = frame::block_content_id(&self.doc, block);
        self.shape_rows.borrow().entries.contains_key(&cid)
    }
    /// （仅测试）当前缓存的基础布局代键（`height_epoch` 结果）——M3 A4 观测点：
    /// 图片占位→真实 / 拖拽改宽走**稀疏覆写**（不进代），代不变；结构性编辑/换宽才换代。
    #[cfg(test)]
    pub(crate) fn block_layout_epoch(&self) -> Option<u64> {
        self.block_layout.borrow().as_ref().map(|(e, _)| *e)
    }
    /// （仅测试）第 `i` 块的基础布局高（不含动态增量）——断言稀疏覆写后的带外/带内
    /// 图片块高（Frame::block_height 只对完整帧，这里读编辑器基表本体）。
    #[cfg(test)]
    pub(crate) fn block_base_height(&self, i: usize) -> Option<f32> {
        self.block_layout.borrow().as_ref().map(|(_, b)| b.borrow().height(i))
    }
    /// （仅测试）Pass2 光栅计数（缓存未命中/光标行时自增）。
    #[cfg(test)]
    pub(crate) fn bump_reshapes(&self) {
        self.reshapes.set(self.reshapes.get() + 1);
    }
    /// （仅测试）Pass2 notdef 冷却帧计数（复用旧几何、未调用 gpui shape）。
    #[cfg(test)]
    pub(crate) fn bump_coolings(&self) {
        self.coolings.set(self.coolings.get() + 1);
    }
    /// （仅测试）D1 预热整形计数（`prewarm_band` 每实际 shape 一块自增）。
    #[cfg(test)]
    pub(crate) fn bump_prewarm_shapes(&self) {
        self.prewarm_shapes.set(self.prewarm_shapes.get() + 1);
    }
    /// （仅测试）自上次读取以来 D1 预热实际整形次数。
    #[cfg(test)]
    pub(crate) fn take_prewarm_shapes(&self) -> u64 {
        let v = self.prewarm_shapes.get();
        self.prewarm_shapes.set(0);
        v
    }
    /// 校验基础布局命中当前（代, 块数）：命中返回 `(false, 新布局)` 供复用；
    /// 未命中建出空布局（build_frame 随即填估算/或 patch）并缓存，返回 `(true, 新)`
    /// 及第三个元素 = **旧布局**（结构编辑增量 Pass1 里「编辑线之前未变前缀」的高度
    /// 复用；首次打开/宽度变化旧布局为空 → `None`）。
    pub(crate) fn ensure_block_layout(
        &self,
        n_blocks: usize,
        epoch: u64,
    ) -> (bool, BlockLayoutRef, Option<BlockLayoutRef>) {
        if let Some((e, l)) = &*self.block_layout.borrow()
            && *e == epoch
            && l.borrow().n() == n_blocks
        {
            return (false, l.clone(), None);
        }
        let old = self.block_layout.borrow().as_ref().map(|(_, l)| l.clone());
        // 重建分支给 **空** 草稿（O(1)）：Pass1 紧随其后整表重建真正填充——不先建一份
        // 全量零点 Fenwick/line_block 又立刻被覆盖（双重 O(n)，Enter/结构编辑每键 ~0.8ms）。
        let l = Rc::new(std::cell::RefCell::new(element::BlockLayout::empty()));
        *self.block_layout.borrow_mut() = Some((epoch, l.clone()));
        (true, l, old)
    }
    /// 折叠/重排计划（缓存）：marker 集合（`FoldPlanSets`）与 `deltas` 分层缓存——
    /// 全部**与可视带无关且不含 caret**。`deltas` 键 = `epoch ^ regions_ver ^ mgen ^
    /// fold_ver`；`sets` 键 + 布局 epoch / mgen（只影响高度）→ `sets` 跨布局复用
    /// （M5-B3-1 子项 2）。滚动/带移动/**光标移动/纯文本键入**都复用同一 `Rc`。
    /// caret 的 reveal-on-caret 由 [`Self::fold_reveal`] 按带现算叠加（最多一个数学块 +
    /// 一个折叠体）。旧实现把 caret 放进键——每次光标动/每键都整算（结构密集大文档
    /// ~40-100ms → 输入/方向键「巨大延迟」）；更早是每档在 `build_frame` 里重算（滚动卡）。
    pub(crate) fn fold_plan(
        &self,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        epoch: u64,
        mgen: u64,
    ) -> Rc<element::FoldPlan> {
        let regions_ver = self.regions_ver();
        let fold_ver = self.fold_ver();
        let key = epoch
            ^ regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
        // marker 身份（不含结构代）：结构编辑后 marker 集合未变 → deltas 可平移复用。
        let marker_key = regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ mgen.wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
        if let Some((k, _m, p)) = &*self.fold_plan.borrow()
            && *k == key
        {
            return p.clone();
        }
        let derived = self.derived();
        // ———（一）marker 集合（`FoldPlanSets`）：从**缓存**取（O(1) `Rc` 复用）——
        // 键 = `regions_ver ^ 行号平移版本 ^ fold_ver`（**不含布局 epoch / mgen**）：
        // 布局换代 / 公式光栅就绪（mgen 变）不改变任何行号/区域结构 → 集合跨计划复用，
        // 不再每键重建 O(#区域) 哈希（M5-B3-1 子项 2）。标题折叠体已在 `derived`
        // 内维护（行号平移 / 折叠态翻转时重扫），不再每键 `text_shared()` 全篇物化
        // + `heading_fold_regions` 全扫（M5-B3-1 子项 4）。
        let sets = self.fold_plan_sets(&derived);
        // ———（二）deltas：marker 身份未变 + 结构编辑待消费 → 增量平移（免 O(n) 双数组
        // 标记 + 0..n 扫描）；否则整建（见 [`Self::fold_plan_deltas_full`]）。
        if let Some(plan) =
            self.fold_plan_deltas_shifted(&derived, base.clone(), key, marker_key, sets.clone())
        {
            return plan;
        }
        self.fold_plan_deltas_full(&derived, base, key, marker_key, sets)
    }
    /// `FoldPlanSets`（marker 集合）构建 + 缓存。键 = `regions_ver ^ regions_shift_ver ^
    /// fold_ver`（**不含布局 epoch / mgen**）：任一键分量表示 region/折叠**结构**变化 →
    /// 重建；epoch/mgen 只改 `deltas`（高度），不碰键 → 布局/公式就绪重建直接复用同一
    /// `Rc`。安全性：集合只含行号与区域下标、不含任何高度值；`math_start` 的 region 下标
    /// 指向 `derived` 内 `math_regions`——键含 `regions_ver` 保证同一 `derived` 实例
    /// （行号平移版本分量保证行号已平移的集合不命中旧键，见 `shift_derived_after_edit`）。
    fn fold_plan_sets(&self, derived: &Rc<RefCell<WyDerived>>) -> Rc<element::FoldPlanSets> {
        let regions_ver = self.regions_ver();
        let shift_ver = self.regions_shift_ver();
        let fold_ver = self.fold_ver();
        let skey = regions_ver.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ shift_ver.wrapping_mul(0x296C_4EA9_6BE3_A5F1)
            ^ fold_ver.wrapping_mul(0x517C_C1B7_2722_0A95);
        if let Some((k, s)) = &*self.fp_sets.borrow()
            && *k == skey
        {
            return s.clone();
        }
        let sets = build_fp_sets(derived);
        *self.fp_sets.borrow_mut() = Some((skey, sets.clone()));
        sets
    }
    /// deltas **增量平移**（B5 + M5-B3-1）：marker 集合来自缓存（O(1) `Rc` 复用 / 行号
    /// 平移，见 [`Self::fold_plan_sets`]），deltas 免 O(n) 双数组标记 + 0..n 扫描
    /// （`fp.dyn/fp.tail` ~3ms debug），改为平移旧计划：块下标小于旧编辑块的不变、
    /// 在旧编辑块之后整体 + 块数净增；被编辑块旧条目丢弃，按新 markers + 新 base 就地
    /// 重算编辑邻域（折叠/数学 body → -height、数学 start → 卡高-height；body 优先）。
    /// 仅在：marker 身份与缓存一致、确有结构编辑待消费时走此路径（**含标题折叠**：
    /// M5-B3-1 把标题折叠体移入 `derived` 行号平移后，平移一致性对折叠态同样成立——
    /// 旧实现因此禁用折叠态时的增量路径）；任何偏离回退 `None` 走整建（正确 > 快）。
    fn fold_plan_deltas_shifted(
        &self,
        derived: &Rc<std::cell::RefCell<WyDerived>>,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        key: u64,
        marker_key: u64,
        sets: Rc<element::FoldPlanSets>,
    ) -> Option<Rc<element::FoldPlan>> {
        if !self.struct_edit_pending.get() {
            return None;
        }
        let old_plan = {
            let old = self.fold_plan.borrow();
            let (_, cm, _p) = old.as_ref()?;
            if *cm != marker_key {
                return None;
            }
            _p.clone()
        };
        let ed_blk_old = self.struct_blk_old.get();
        let blk_delta = self.struct_blk_span_delta.get();
        // 平移旧 deltas（块下标）；被编辑块旧条目丢弃，由编辑邻域重算补充。
        let mut deltas: Vec<(usize, f32)> = Vec::with_capacity(old_plan.deltas.len() + 2);
        for &(ix, v) in old_plan.deltas.iter() {
            if ix < ed_blk_old {
                deltas.push((ix, v));
            } else if ix > ed_blk_old {
                deltas.push(((ix as i64 + blk_delta).max(0) as usize, v));
            }
        }
        // 编辑邻域（受影响的新块区间）就地重算。
        let base_br = base.borrow();
        let n = base_br.n();
        let ed_blk_new = self
            .doc
            .block_index_for_line(self.struct_dirty_line())
            .min(n.saturating_sub(1));
        let fold_lines = &sets.fold_lines;
        let math_body = &sets.math_body;
        let math_start = &sets.math_start;
        let member_delta = |ix: usize| -> Option<f32> {
            let m = self.doc.block_meta(ix)?;
            let s0 = m.src_line as usize;
            let s1 = (s0 + m.aux.max(1) as usize).min(self.doc.line_starts().len());
            let mut line = s0;
            while line < s1 {
                if fold_lines.contains(&line) || math_body.contains(&line) {
                    return Some(-base_br.height(ix));
                }
                line += 1;
            }
            let mut line = s0;
            while line < s1 {
                if let Some(&ridx) = math_start.get(&line) {
                    let card_h = {
                        let d = derived.borrow();
                        f32::from(element::math_card_height(self, &d.math_regions[ridx]))
                    };
                    return Some(card_h - base_br.height(ix));
                }
                line += 1;
            }
            None
        };
        let lo = ed_blk_new.saturating_sub(1);
        let hi = (ed_blk_new + blk_delta.max(0) as usize + 2).min(n);
        for ix in lo..hi {
            if let Some(v) = member_delta(ix) {
                if let Some(pos) = deltas.iter().position(|&(b, _)| b == ix) {
                    deltas.remove(pos);
                }
                deltas.push((ix, v));
            }
        }
        deltas.sort_unstable_by_key(|&(b, _)| b);
        deltas.dedup_by_key(|&mut (b, _)| b);
        let delta_sum: f32 = deltas.iter().map(|&(_, v)| v).sum();
        let delta_pref = element::build_delta_pref(&deltas);
        let plan = Rc::new(element::FoldPlan {
            sets,
            deltas: deltas.into_boxed_slice().into(),
            delta_pref,
            delta_sum,
        });
        self.struct_edit_pending.set(false); // 消费本次平移
        *self.fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
        Some(plan)
    }
    /// deltas **整建**：marker 集合（取自缓存）→「命中标记 + 块值」双数组（块下标 ∈
    /// [0,n)，填表 O(1)、尾随按 0..n 升序扫出——免 30k 项全排序；见 `fp.dyn/fp.tail`）。
    fn fold_plan_deltas_full(
        &self,
        derived: &Rc<std::cell::RefCell<WyDerived>>,
        base: Rc<std::cell::RefCell<element::BlockLayout>>,
        key: u64,
        marker_key: u64,
        sets: Rc<element::FoldPlanSets>,
    ) -> Rc<element::FoldPlan> {
        let math_regions = &derived.borrow().math_regions;
        let _t_dyn = crate::prof::begin();
        let base_br = base.borrow();
        let nb = base_br.n();
        let mut marked: Vec<bool> = vec![false; nb];
        let mut vals: Vec<f32> = vec![0.0; nb];
        for &srcline in sets.fold_lines.iter() {
            let ix = base_br.block_for_line(srcline);
            marked[ix] = true;
            vals[ix] = -base_br.height(ix);
        }
        for &srcline in sets.math_body.iter() {
            let ix = base_br.block_for_line(srcline);
            marked[ix] = true;
            vals[ix] = -base_br.height(ix);
        }
        for (&srcline, &ridx) in sets.math_start.iter() {
            let ix = base_br.block_for_line(srcline);
            if !marked[ix] {
                let card_h = f32::from(element::math_card_height(self, &math_regions[ridx]));
                marked[ix] = true;
                vals[ix] = card_h - base_br.height(ix);
            }
        }
        crate::prof::end("wy.fp.dyn", _t_dyn);
        let _t_tail = crate::prof::begin();
        let mut deltas_vec: Vec<(usize, f32)> = Vec::new();
        for ix in 0..nb {
            if marked[ix] {
                deltas_vec.push((ix, vals[ix]));
            }
        }
        let delta_sum: f32 = deltas_vec.iter().map(|&(_, v)| v).sum();
        crate::prof::end("wy.fp.tail", _t_tail);
        let delta_pref = element::build_delta_pref(&deltas_vec);
        let plan = Rc::new(element::FoldPlan {
            sets,
            deltas: deltas_vec.into_boxed_slice().into(),
            delta_pref,
            delta_sum,
        });
        self.struct_edit_pending.set(false);
        *self.fold_plan.borrow_mut() = Some((key, marker_key, plan.clone()));
        plan
    }
    /// caret 所在区域「展开显示源码」（reveal-on-caret）覆盖，按带现算（O(log #区域)
    ///   二分定位数学块 + 折叠体，少量块定位）——最多一个数学块 + 一个折叠体。
    ///   `build_frame` 用它把全折叠基准里该区域的行恢复成可编辑源码显示。
    pub(crate) fn fold_reveal(
        &self,
        caret_line: usize,
        plan: &element::FoldPlan,
        base: &std::cell::RefCell<element::BlockLayout>,
    ) -> element::RevealPlan {
        let mut reveal = element::RevealPlan::empty();
        let base_br = base.borrow();
        let block_ix_of =
            |srcline: usize| -> usize { base_br.block_index_for_src_line(srcline).unwrap_or(0) };
        let zero_ix = |zeroed: &mut Vec<usize>, ix: usize| {
            if !zeroed.contains(&ix) {
                zeroed.push(ix);
            }
        };
        // 数学块：caret 在 [start, end) → 整块还原源码（不卡片、body 不折叠 0 高）。
        // `math_regions` 升序且互不重叠 → 二分（O(log n)，替代每帧线性扫）。
        {
            let d = self.derived();
            let d = d.borrow();
            if let Some(ridx) = region_containing(&d.math_regions, |r| r.range.clone(), caret_line, true) {
                let reg = &d.math_regions[ridx];
                let rng = reg.range.clone();
                reveal.ranges.push(rng.clone());
                zero_ix(&mut reveal.zeroed, block_ix_of(rng.start));
                for l in rng.start + 1..rng.end {
                    zero_ix(&mut reveal.zeroed, block_ix_of(l));
                }
            }
        }
        // 折叠体：caret 在 body `[start+1, end)` → 该体行还原源码（不折叠成 0）。
        // `fold_bodies` 两组各自升序不重叠、跨组可重叠 → `folded_body_at` 分组二分
        // （alert 优先），与旧线性 `find` 首中语义一致。
        if let Some(rnge) = folded_body_at(&plan.sets.fold_bodies, plan.sets.fold_split, caret_line) {
            let rng = rnge.clone();
            reveal.ranges.push(rng.clone());
            for l in rng.start + 1..rng.end {
                zero_ix(&mut reveal.zeroed, block_ix_of(l));
            }
        }
        // 被归零的静态 Δ 之和（total 减去）。
        if !reveal.zeroed.is_empty() {
            for &ix in &reveal.zeroed {
                if let Some((_, v)) = plan.deltas.iter().find(|(i, _)| *i == ix) {
                    reveal.reveal_delta_sum += v;
                }
            }
        }
        reveal
    }
    /// 内容派生扫描缓存（全篇一遍，按 `regions_ver` 缓存）——`build_frame` / paint 用，
    /// 避免滚动/每帧重复全篇扫描（大文档卡顿主因之一）。**标记计数器预筛**（增量维护，
    /// 见 `replace_range`）：无 `$` → `math_regions` 空；无 `[!` → 两个 alert 扫描空。
    /// 7MB 大文档 `math_regions` 全篇行切分+扫描 ~390ms——**纯文本键入**（region 签名
    /// 不变）不递增 `regions_ver` → 跨键复用 `Rc`，编辑每键省掉这些全篇扫描
    /// （结构密集文档「输入卡死」的根因，旧实现每键重扫 ~1s）。
    pub(crate) fn derived(&self) -> Rc<RefCell<WyDerived>> {
        let ver = self.regions_ver.get();
        if let Some((g, d)) = &*self.derived.borrow()
            && *g == ver
        {
            return d.clone();
        }
        // Cut-1b：三个全篇扫描的**计数器预筛全空**（无 `$`、无 `[!`、无折叠标题）时，
        // derived 必为全空三表——直接返回，**不物化全文**。这是跨行删除/合并回退整建
        // （`regions_ver++`）后 `derived()` 重建里最贵的一步（6.4MB rope → String 的
        // `text_shared()` 全拷 ≈ 16ms/次），纯文本无区域时纯属浪费。语义与「物化后由
        // 空扫描得空表」完全一致（三个分支各自在预筛为空时即返空，见下）。
        if self.math_dollar_cnt.get() == 0
            && self.alert_marker_cnt.get() == 0
            && self.folded_headings.borrow().is_empty()
        {
            let d = Rc::new(RefCell::new(WyDerived {
                math_regions: Vec::new(),
                alert_fold_regions: Vec::new(),
                alert_region_kinds: Default::default(),
                heading_fold_bodies: Vec::new(),
            }));
            *self.derived.borrow_mut() = Some((ver, d.clone()));
            return d;
        }
        let content = self.doc.text_shared();
        // 警报区域单遍扫描，两个公开查询由它派生（旧实现 `alert_fold_regions` +
        // `alert_region_kinds` 各扫一遍全篇——重复 O(n)）。
        let alerts = if self.alert_marker_cnt.get() > 0 {
            inline_style::alert_regions(&content)
        } else {
            Vec::new()
        };
        let alert_fold_regions = alerts
            .iter()
            .filter_map(|(r, _, fold)| fold.map(|f| (r.clone(), f)))
            .collect();
        let mut alert_region_kinds: rustc_hash::FxHashMap<
            usize,
            crate::markdown::parse::recognition::AlertKind,
        > = rustc_hash::FxHashMap::with_capacity_and_hasher(alerts.len() * 2, Default::default());
        for (r, k, _) in &alerts {
            for l in r.clone() {
                alert_region_kinds.insert(l, *k);
            }
        }
        let d = Rc::new(RefCell::new(WyDerived {
            math_regions: if self.math_dollar_cnt.get() > 0 {
                inline_style::math_regions(&content)
            } else {
                Vec::new()
            },
            alert_fold_regions,
            alert_region_kinds,
            heading_fold_bodies: {
                let folded = self.folded_headings.borrow();
                if folded.is_empty() {
                    Vec::new()
                } else {
                    inline_style::heading_fold_regions(&content, &folded)
                }
            },
        }));
        *self.derived.borrow_mut() = Some((ver, d.clone()));
        d
    }
}

/// 在**按 start 升序且互不重叠**的区域表上二分定位「包含 `line` 的区域」下标
/// （O(log #区域)，替代逐区域线性扫）。`regions` 可为 `&[Range<usize>]`（如
/// `fold_bodies` 两组）或 `&[MathRegion]`（`range_of` 抽出 `Range<usize>`）。
/// `math_regions` 由前向扫描建、行号平移单调 → 保持升序与互不重叠 → 满足前提。
/// `incl_start`：起始行算不算“包含”（数学 `>= start`；折叠体用 `> start`，标题行/
/// 标记行本身不折叠）。原理只查「最后一个 start ≤/&lt; line 的区域」——表内重叠时该
/// 结论不成立，**重叠或无序的表不要用它**（`fold_bodies` 跨 alert/标题组可重叠 →
/// 走 [`fold_reveal`] 的分组二分）。
pub(crate) fn region_containing<T>(
    regions: &[T],
    range_of: impl Fn(&T) -> Range<usize>,
    line: usize,
    incl_start: bool,
) -> Option<usize> {
    let p = if incl_start {
        regions.partition_point(|r| range_of(r).start <= line)
    } else {
        regions.partition_point(|r| range_of(r).start < line)
    };
    let idx = p.checked_sub(1)?;
    (line < range_of(&regions[idx]).end).then_some(idx)
}

/// `fold_bodies`（= [alert 折叠体组 ‖ 标题折叠体组]）里「包含 caret 的折叠体」。两组
/// 各自升序且互不重叠 → 各二分（O(log #折叠体)）；跨组可重叠（折叠标题体可包住折叠
/// callout），按旧线性 `find` 的「alert 组优先首中」语义返回。
fn folded_body_at(
    fold_bodies: &[Range<usize>],
    fold_split: usize,
    line: usize,
) -> Option<&Range<usize>> {
    fold_bodies
        .get(..fold_split)
        .and_then(|g| region_containing(g, |r| r.clone(), line, false).map(|i| &g[i]))
        .or_else(|| {
            fold_bodies
                .get(fold_split..)
                .and_then(|g| region_containing(g, |r| r.clone(), line, false).map(|i| &g[i]))
        })
}

/// 由已增量维护的 `WyDerived` 构建 marker 集合（O(#数学块 + #折叠体) 预分配哈希构建）。
/// 标题折叠体不再每键扫全篇文本：`heading_fold_bodies` 已由 `shift_derived_after_edit`
/// 行号平移 / `toggle_heading_fold_at_line` / `derived`（`regions_ver` 重建）维护在
/// `WyDerived` 内（M5-B3-1 子项 4，见 [`WyDerived::heading_fold_bodies`] 注释）。
fn build_fp_sets(derived: &Rc<RefCell<WyDerived>>) -> Rc<element::FoldPlanSets> {
    let d = derived.borrow();
    let math_regions = &d.math_regions;
    // 预分配容量（大文档 3000 数学块：未预分配每次重建多次 rehash ≈20ms）。
    let _t_sets = crate::prof::begin();
    let mut math_start: rustc_hash::FxHashMap<usize, usize> =
        rustc_hash::FxHashMap::with_capacity_and_hasher(
            math_regions.len(),
            std::default::Default::default(),
        );
    let mut math_body: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::with_capacity_and_hasher(
        math_regions.len() * 2,
        std::default::Default::default(),
    );
    for (idx, reg) in math_regions.iter().enumerate() {
        math_start.insert(reg.range.start, idx);
        math_body.extend(reg.range.start + 1..reg.range.end);
    }
    let mut fold_lines: rustc_hash::FxHashSet<usize> = rustc_hash::FxHashSet::default();
    let mut fold_bodies: Vec<Range<usize>> = Vec::new();
    for (rnge, folded) in &d.alert_fold_regions {
        if !*folded {
            continue;
        }
        fold_bodies.push(rnge.clone());
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    // alert 折叠体条数 = `fold_bodies` 前缀长度（两组二分分组边界：alert 组在前）。
    let fold_split = fold_bodies.len();
    for rnge in &d.heading_fold_bodies {
        fold_bodies.push(rnge.clone());
        fold_lines.extend(rnge.start + 1..rnge.end);
    }
    crate::prof::end("wy.fp.sets", _t_sets);
    Rc::new(element::FoldPlanSets {
        math_start,
        math_body,
        fold_lines,
        fold_bodies,
        fold_split,
    })
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    /// 线性参考：区域表逐条找首个命中（与生产二分前一致）。
    fn linear_containing(regions: &[Range<usize>], line: usize, incl_start: bool) -> Option<usize> {
        regions.iter().position(|r| {
            if incl_start {
                line >= r.start && line < r.end
            } else {
                line > r.start && line < r.end
            }
        })
    }

    /// M3-A3 对拍：`region_containing` 二分 == 线性参考，覆盖空表、空区间、紧邻、
    /// 单行、区域前后/边界/间隙各形态。
    #[test]
    fn region_containing_matches_linear_reference() {
        let regions: Vec<Range<usize>> = vec![
            0..3,
            3..3, // 空区间（不应命中任何行）
            5..6, // 单行
            8..8,
            9..14,
            14..17,
            20..21,
            24..30,
        ];
        assert_eq!(
            super::region_containing(&[], |r: &Range<usize>| r.clone(), 3, true),
            None,
            "空表"
        );
        for incl in [true, false] {
            for line in 0..35usize {
                assert_eq!(
                    super::region_containing(&regions, |r| r.clone(), line, incl),
                    linear_containing(&regions, line, incl),
                    "incl_start={incl} line={line}: 二分 != 线性参考"
                );
            }
        }
    }

    /// M3-A3 对拍：`folded_body_at` 分组二分 == 旧线性 `find` 首中；含跨组重叠
    /// （折叠标题体包住折叠 callout）与 alert 优先语义。
    #[test]
    fn folded_body_at_matches_linear_find_with_cross_group_overlap() {
        // 构造序 = 建表序：alert 折叠体组（前）‖ 标题折叠体组（后）。
        let bodies: Vec<Range<usize>> = vec![
            10..16, // alert 组 0
            30..60, // alert 组 1（包住标题 40..50）
            70..71, // alert 组 2
            2..20, // 标题组 0（包住 alert 10..16）
            40..50, // 标题组 1
        ];
        let split = 3usize;
        for line in 0..80usize {
            let bin = super::folded_body_at(&bodies, split, line).cloned();
            let lin = bodies
                .iter()
                .find(|r| line > r.start && line < r.end)
                .cloned();
            assert_eq!(bin, lin, "line={line}: 分组二分 {bin:?} != 线性首中 {lin:?}");
        }
        // 关键边界抽查：caret 在标题体内但 callout 外（旧行为=标题体，不能错收成 alert）。
        let b = [10..16, 2..20usize];
        assert_eq!(super::folded_body_at(&b, 1, 3).cloned(), Some(2..20));
        assert_eq!(super::folded_body_at(&b, 1, 16).cloned(), Some(2..20));
        assert_eq!(super::folded_body_at(&b, 1, 2).cloned(), None, "标题行本身不折叠");
        assert_eq!(super::folded_body_at(&b, 1, 20).cloned(), None);
    }
}
