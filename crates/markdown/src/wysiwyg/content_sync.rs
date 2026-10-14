use super::*;

use std::sync::Arc;

use gpui::RenderImage;

use crate::markdown::wysiwyg::math_typeset;

/// WyEditor 数学排版 / mermaid 缩放 / 代码高亮后台解析（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /* ---------- 数学排版（LaTeX → RenderImage；参考 zorite 的 MathStore） ---------- */

    /// 公式排版槽（元素 build/paint 只读查询）。
    pub(crate) fn math_slots(
        &self,
    ) -> Rc<RefCell<std::collections::HashMap<SharedString, MathSlot>>> {
        self.math_slots.clone()
    }

    /// 排版完成代数（Loading→Ready/Failed 自增，纳入帧缓存键）。
    pub(crate) fn math_gen(&self) -> u64 {
        self.math_gen.get()
    }

    /// 读某块 `$$` 卡的「最近就绪位图」钉（`src_line` → 位图 + 尺寸）。
    pub(crate) fn math_pin(&self, src_line: usize) -> Option<BlockMathPin> {
        self.math_pins.borrow().get(&src_line).cloned()
    }

    /// 录制/更新某块 `$$` 卡的最近就绪位图钉（Ready 命中或就绪绘制时调用）。
    pub(crate) fn set_math_pin(&self, src_line: usize, image: Arc<RenderImage>, w: f32, h: f32) {
        self.math_pins
            .borrow_mut()
            .insert(src_line, BlockMathPin { image, w, h });
    }

    /// 清空数学卡位图钉（换源 / 结构编辑 / 主题色变化：行号或颜色失效）。
    pub(crate) fn clear_math_pins(&self) {
        self.math_pins.borrow_mut().clear();
    }
    /* ---------- Mermaid 图：滚轮逐块缩放（缩放值随块源码行存，重排用） ---------- */

    /// 某 mermaid 围栏块的缩放（缺省 1.0）。
    pub(crate) fn mermaid_zoom(&self, src_line: usize) -> f32 {
        self.mermaid_zoom
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(1.0)
    }

    /// 设置某围栏块缩放并自增缩放代（纳入帧缓存/基础布局代 → 整帧+块高按新比例重建）。
    pub(crate) fn set_mermaid_zoom(&self, src_line: usize, z: f32) {
        self.mermaid_zoom.borrow_mut().insert(src_line, z);
        self.mermaid_zoom_gen
            .set(self.mermaid_zoom_gen.get().wrapping_add(1));
    }

    /// mermaid 缩放代数（进帧缓存键 + 基础布局代）。
    pub(crate) fn mermaid_zoom_gen(&self) -> u64 {
        self.mermaid_zoom_gen.get()
    }

    /// 当前获得焦点的 mermaid 卡（`src_line`）；`None` = 无（滚轮在 mermaid 上将继续滚动）。
    pub(crate) fn focused_mermaid(&self) -> Option<usize> {
        self.focused_mermaid
    }

    /// 数学源码**显示行→卡高估计**缓存（键=源）：`math_line_height_h` 对每源做
    /// LaTeX→Unicode 转换与高算子扫描，3000 块每次 `fold_plan` 重建重复 ≈38ms；
    /// 按源缓存后 O(1) 复用（同一源估计值确定不变）。
    pub(crate) fn math_height_estimate(&self, src: &str) -> f32 {
        if let Some(&h) = self.math_line_map.borrow().get(src) {
            return h;
        }
        let h = element::math_line_height_h(src);
        if self.math_line_map.borrow().len() < 10_000 {
            self.math_line_map.borrow_mut().insert(src.to_string(), h);
        }
        h
    }
    /* ---------- 代码块高亮：后台 syntect 整块解析（把 debug 下 ~300ms/块移出主线程） ---------- */

    /// 查代码块高亮解析缓存：就绪返回高亮 run，否则按 NotReady 处理。
    pub(crate) fn code_parse_lookup(&self, lang: &str, code: &str) -> CodeParseLookup {
        let m = self.code_parse.lock().unwrap();
        let key = (lang.into(), code.into());
        match m.get(&key) {
            Some(CodeParseState::Done(runs)) => CodeParseLookup::Ready(runs.clone()),
            _ => CodeParseLookup::NotReady,
        }
    }

    /// shape 期登记一个需要后台解析的代码块（幂等：已在途/已就绪则跳过）。
    /// `src_line` 供完成时的带内门控使用。
    pub(crate) fn request_code_parse(&self, lang: &str, code: &str, src_line: usize) {
        let key: CodeParseKey = (lang.into(), code.into());
        {
            let mut m = self.code_parse.lock().unwrap();
            if !m.contains_key(&key) {
                m.insert(key.clone(), CodeParseState::Pending);
            }
        }
        // 记录**每个**请求块的 src_line：同一内容键可被多个内容完全相同的代码块请求
        // （sync_code_parse 按键分组、就绪时对全部同内容块定向作废缓存，否则滚回带内
        // 第二个相同副本因 request 去重永不重上色、永久保持纯文本）。
        let mut need = self.code_parse_need.borrow_mut();
        if !need.iter().any(|(k, s)| k == &key && *s == src_line) {
            need.push((key, src_line));
        }
    }

    /// 消费本帧登记的代码块后台解析请求（paint 期调用，对齐 `sync_math` 的可见带
    /// 懒解析）。syntect 整块解析是纯计算 → background executor 跑，完成后写缓存
    /// 并定向作废该块整形缓存 + 失效帧 → 主线程下一帧用高亮行重建该块（不再
    /// ~300ms 阻塞滚动/整形帧）。
    pub(crate) fn sync_code_parse(&mut self, cx: &mut Context<Self>) {
        let jobs = std::mem::take(&mut *self.code_parse_need.borrow_mut());
        if jobs.is_empty() {
            return;
        }
        // 按内容键分组（同一键可由多个相同代码块请求）：每个键只 spawn 一个解析任务，
        // 完成时对全部同内容块的 src_line 一并定向作废再上色。
        let mut groups: Vec<(CodeParseKey, Vec<usize>)> = Vec::new();
        for (key, src_line) in jobs {
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, lines)) if !lines.contains(&src_line) => lines.push(src_line),
                Some(_) => {}
                None => groups.push((key, vec![src_line])),
            }
        }
        for ((lang, code), src_lines) in groups {
            let cache = self.code_parse.clone();
            cx.spawn(async move |this, cx| {
                // 内层 async move 需要 'static 自有值：克隆一份进去，外层保留原值插缓存。
                let lang2 = lang.clone();
                let code2 = code.clone();
                let result: Option<syntax::CodeLineRuns> = cx
                    .background_executor()
                    .spawn(async move {
                        syntax::parse_code_lines(lang2.as_ref(), code2.as_ref())
                    })
                    .await;
                {
                    let mut m = cache.lock().unwrap();
                    m.insert((lang.clone(), code.clone()), CodeParseState::Done(result));
                }
                let _ = this.update(cx, |e, ecx| e.on_code_parse_ready(&src_lines, ecx));
            })
            .detach();
        }
    }

    /// 一个/多个代码块高亮就绪：**定向作废这些块的行整形缓存条目**（替代旧
    /// 「`code_parse_gen` 恒自增 + 进全局 shape_key」：全局键在代码块内每键打字（内容变 →
    /// 新内容解析完成 → gen++）时会让**整带所有块**（含无关段落）全量 miss 重排，真实
    /// 窗口顶部密集带 24-34ms/键、`bf.shape` 6-8ms）。改动后仅这些代码块（的旧纯文本
    /// 缓存行）被删，本帧/滚回带内由 Pass2 以新高亮重排该块，其余块保持命中。
    /// `src_lines` 可能是多个（内容完全相同的重复代码块共享同一解析键）。仅当任一
    /// src_line 仍在本帧带内（`frame.rows` 含它）才立即 `frame=None + notify` 强制本帧
    /// 换高亮——带外完成只删缓存、不打断帧（对齐 `on_math_ready` 的带内门控，消除多块
    /// 并发完成时的重建 churn）。
    pub(crate) fn on_code_parse_ready(&mut self, src_lines: &[usize], cx: &mut Context<Self>) {
        let block_indices: Vec<usize> = src_lines
            .iter()
            .map(|&s| self.doc.block_index_for_line(s))
            .collect();
        let ids: Vec<u64> = block_indices
            .iter()
            .map(|&b| frame::block_content_id(&self.doc, b))
            .collect();
        self.shape_rows.borrow_mut().invalidate_ids(&ids);
        let in_band = self
            .frame
            .borrow()
            .as_ref()
            .is_some_and(|f| f.rows.iter().any(|r| src_lines.contains(&r.src_line)));
        if in_band {
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }
    /// 对**当前带内可见**公式发起离屏排版——按「只渲染窗口可见区域」原则（对齐
    /// `view`/`editor`）：不再开篇全量 claim 全篇 ~1.3 万公式。旧实现每个公式后台
    /// 光栅 10-20ms（debug），全量累计数百 CPU-秒 → 打满后台线程、UI 帧被拖慢 →
    /// 「还是很卡顿」。
    ///
    /// 消费**窗口化 `frame.rows`**（已整形的带内行：`extra.card = Math{latex}` 是块
    /// 卡片、`extra.inline_maths[].latex` 是行内公式）——滚入带的公式由 shape/splice
    /// 带出 → 这里按需 claim → 就绪后 `mgen++` 重建帧（卡片/行内 spacer 换真位图）。
    /// 幂等：已占用槽（含 Loading/Failed）不重复发起；主题色变化清缓存强制重排。
    /// 每次 paint 前调一次成本 = iterate `frame.rows`（≤ 带内行数）+ contains_key。
    pub(crate) fn sync_math(&mut self, cx: &mut Context<Self>, frame: &element::Frame) {
        let color = math_color(&self.theme);
        let color_changed = self.math_color.borrow().as_ref() != Some(&color);
        if color_changed {
            self.math_color.borrow_mut().replace(color);
            self.math_slots.borrow_mut().clear();
            // 主题色变化：旧位图颜色失效 → 钉一并清空（否则 Loading 期会画旧色）。
            self.clear_math_pins();
        }
        // 收集带内块 + 行内公式源（键 = 去定界符后的源码，同 zorite 的 MathStore）。
        // (槽键, 是否行内, 排版边距, 要排版的 LaTeX)：块用 `MATH_PAD`；行内用
        // `INLINE_MATH_PAD` 且键带 `inline:` 前缀 + `\textstyle` 包层（大算子转行内式，
        // 只压算子不缩正文——否则 `\int_0^1` 整图缩放会把 `x^2 dx` 也变小）。
        let mut sources: Vec<(SharedString, bool, f32, String, usize)> = Vec::new();
        for row in &frame.rows {
            let src_line = row.src_line;
            if let Some(element::BlockCard::Math { latex, .. }) = &row.extra.card {
                let s = latex.trim();
                if !s.is_empty() && s != "$$" {
                    sources.push((
                        s.to_string().into(),
                        false,
                        config::math::MATH_PAD,
                        s.to_string(),
                        src_line,
                    ));
                }
            }
            for m in &row.extra.inline_maths {
                let lx = m
                    .key
                    .as_ref()
                    .strip_prefix("inline:")
                    .unwrap_or(m.key.as_ref());
                if !lx.is_empty() {
                    sources.push((
                        m.key.clone(),
                        true,
                        config::math::INLINE_MATH_PAD,
                        lx.to_string(),
                        src_line,
                    ));
                }
            }
        }
        for (key, is_inline, pad, latex, src_line) in sources {
            // 行内公式登记「所在行」→ 完成时定向作废该行整形缓存（见 `inline_math_rows`）。
            if is_inline {
                let mut rows = self.inline_math_rows.borrow_mut();
                let v = rows.entry(key.clone()).or_default();
                if !v.contains(&src_line) {
                    v.push(src_line);
                }
                drop(rows);
            }
            let mut slots = self.math_slots.borrow_mut();
            match slots.get(key.as_ref()) {
                Some(MathSlot::Ready(img, w, h)) => {
                    // 已就绪（滚动回带/刷新帧）：同步录制该块「最近就绪位图」钉，
                    // 使后续逐键 Loading 期有旧图可钉（防 Unicode 回退闪跳）。
                    let pin = (img.clone(), *w, *h);
                    drop(slots);
                    self.set_math_pin(src_line, pin.0, pin.1, pin.2);
                    continue;
                }
                // Loading/Failed 已占用：幂等不重复发起。
                Some(_) => continue,
                None => {}
            }
            slots.insert(key.clone(), MathSlot::Loading);
            drop(slots);
            // 离屏排版（纯 CPU；与 zorite `ensure_math_loaded` 同构）。
            let slots = self.math_slots.clone();
            let color2 = color;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        // 行内先试 `\textstyle …`（大算子转上下标、只压算子）；包层解析
                        // 失败（如某些环境命令）则回退原样排版，避免把好公式降级成 Unicode。
                        if is_inline {
                            let ts = format!("\\textstyle {latex}");
                            math_typeset::render_latex_with_pad(
                                &ts,
                                config::math::MATH_FONT_SIZE,
                                config::math::MATH_DPR,
                                color2,
                                pad,
                            )
                            .or_else(|| {
                                math_typeset::render_latex_with_pad(
                                    &latex,
                                    config::math::MATH_FONT_SIZE,
                                    config::math::MATH_DPR,
                                    color2,
                                    pad,
                                )
                            })
                        } else {
                            math_typeset::render_latex_with_pad(
                                &latex,
                                config::math::MATH_FONT_SIZE,
                                config::math::MATH_DPR,
                                color2,
                                pad,
                            )
                        }
                        .map(|r| (r.image, r.width, r.height))
                    })
                    .await;
                let slot = match result {
                    Some((img, w, h)) => MathSlot::Ready(img, w, h),
                    None => MathSlot::Failed,
                };
                let slots2 = slots.clone();
                let key2 = key.clone();
                // 完成时自增 `math_gen`（on_math_ready）使帧/折叠计划按**真实位图高**重建：
                // 这里 key 必已以 `Loading` 占位 → `insert` 返回 `Some(Loading)`，用
                // `map_or(false, …Loading)` 判定“排版完成”（旧的 `.is_none()` 恒假——
                // 占位符永远在场，mgen 从不自增 → 卡片永远停在回退高 42px，68px 位图
                // 越出灰底——本 bug 自 RaTeX 接入起即存在）。
                let became_ready = slots2
                    .borrow_mut()
                    .insert(key.clone(), slot)
                    .is_some_and(|old| matches!(old, MathSlot::Loading));
                if became_ready {
                    let _ = this.update(cx, |e, ecx| e.on_math_ready(&key2, ecx));
                }
            })
            .detach();
        }
    }

    /// 一个公式排版完成：自增代数 → 帧缓存失效（下次 measure 以真实高度重建）。
    /// **定向作废**：行内公式（`"inline:…"`）→ 删除包含它的行整形缓存条目（行内
    /// spacer 宽/撑高依赖 slot 由 Loading→Ready 换代，不删则滚回带内命中旧行永不换
    /// 真图）。`$$` 显示块行在 Pass2 被 blocked、不入 `shape_rows`，无需作废。替代旧
    /// 「`mgen` 进全局 shape_key」——每个公式完成会让整带所有块（含无关段落）全 miss
    /// 重排一次（headless 实测 67/68 行整带重排）。
    /// **带门控**：完成的公式不在当前带内 → 只置代数 + 定向作废（保持候选高度一致，
    /// 滚动回带时由自然重建带上），不立即失效帧/重绘——滚过一片公式区时几十个后台光栅
    /// 完成不再各自打断一帧（消除数学密集段的滚动/空转卡顿）。在带内才立即失效（本帧换真图）。
    pub(crate) fn on_math_ready(&mut self, key: &str, cx: &mut Context<Self>) {
        if key.starts_with("inline:") {
            let mut lines: Vec<usize> = Vec::new();
            if let Some(v) = self.inline_math_rows.borrow().get(key) {
                lines.extend(v.iter().copied());
            }
            if let Some(f) = self.frame.borrow().as_ref() {
                for r in &f.rows {
                    if r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key) {
                        lines.push(r.src_line);
                    }
                }
            }
            if !lines.is_empty() {
                lines.sort_unstable();
                lines.dedup();
                let blocks: Vec<usize> = lines
                    .into_iter()
                    .map(|s| self.doc.block_index_for_line(s))
                    .collect();
                let ids: Vec<u64> = blocks
                    .iter()
                    .map(|&b| frame::block_content_id(&self.doc, b))
                    .collect();
                self.shape_rows.borrow_mut().invalidate_ids(&ids);
            }
        }
        let in_band = self.frame.borrow().as_ref().is_some_and(|f| {
            f.rows.iter().any(|r| {
                if let Some(element::BlockCard::Math { latex, .. }) = &r.extra.card
                    && latex.trim() == key
                {
                    return true;
                }
                r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key)
            })
        });
        self.math_gen.set(self.math_gen.get().wrapping_add(1));
        if in_band {
            *self.frame.borrow_mut() = None;
            cx.notify();
        }
    }
}
