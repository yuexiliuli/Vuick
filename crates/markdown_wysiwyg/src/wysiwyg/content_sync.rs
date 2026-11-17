use super::*;

use std::sync::Arc;

use gpui::RenderImage;

use markdown_wysiwyg_math;

/// WyEditor 数学排版 / mermaid 缩放 / 代码高亮后台解析（M2.1 自 mod.rs 拆出）。
impl WyEditor {
    /* ---------- 数学排版（LaTeX → RenderImage；参考 zorite 的 MathStore） ---------- */

    /// 把一张「应丢弃」的 Ready 位图登记进 `pending_math_drop`（下一帧 `sync_math`
    /// 统一 `drop_image` 释放）。三点共用（换源守卫不新鲜 / 占位槽被淘汰 / 主题色换变）
    /// ——收敛「散点直 push」为单一入口（K10 副作用集中注入；K14 的 `cx.defer`/
    /// 帧末统一释放姿势不变）。
    fn queue_math_drop(&self, img: Arc<RenderImage>) {
        self.pending_math_drop.borrow_mut().push(img);
    }

    /// 换源脏标记消费：置脏的下一帧经 [`WyEditor::render`]（`impl Render`，宿主每帧
    /// 重绘入口，带 `window`）到这里统一 `clear` 释放旧文档图片纹理。——`install_open`
    /// 无 cx/window，gpui `RetainAllImageCache::clear` 需要 `&mut Window`，故延迟到每帧；
    /// 与 `sync_math` 的 `pending_math_drop` **同款「脏标记 + 每帧统一 flush」姿势**
    ///（K10 副作用集中注入）。仅消费标志，无脏返回；换源全清正确——此刻新源图片尚未
    /// load，缓存里全是旧文档图。
    pub(crate) fn flush_image_cache_drop(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.image_cache_dirty.replace(false) {
            self.image_cache.update(cx, |c, cx| c.clear(window, cx));
        }
    }

    /// 公式排版槽（元素 build/paint 只读查询）。
    pub(crate) fn math_slots(&self) -> Rc<RefCell<MathSlotCache>> {
        self.math_slots.clone()
    }

    /// 排版完成代数（Loading→Ready/Failed 自增，纳入帧缓存键）。
    pub(crate) fn math_gen(&self) -> u64 {
        self.rd.borrow().math_gen.get()
    }

    /// 读某块 `$$` 卡的「最近就绪位图」钉（`src_line` → 位图 + 尺寸）。
    /// 仅供宿主测试消费的旧签名转发（生产无调用）→ allow 压 dead_code 告警。
    #[allow(dead_code)]
    pub(crate) fn math_pin(&self, src_line: usize) -> Option<BlockMathPin> {
        self.rd.borrow().math_pins.borrow().get(&src_line).cloned()
    }

    /// 录制/更新某块 `$$` 卡的最近就绪位图钉（Ready 命中或就绪绘制时调用）——
    /// 算法体已随引擎迁 `markdown_wysiwyg_engine::frame::set_math_pin`。
    pub(crate) fn set_math_pin(&self, src_line: usize, image: Arc<RenderImage>, w: f32, h: f32) {
        markdown_wysiwyg_engine::frame::set_math_pin(&self.rd, src_line, image, w, h);
    }

    /// 清空数学卡位图钉（换源 / 结构编辑 / 主题色变化：行号或颜色失效）。
    pub(crate) fn clear_math_pins(&self) {
        self.rd.borrow_mut().math_pins.borrow_mut().clear();
    }
    /* ---------- Mermaid 图：滚轮逐块缩放（缩放值随块源码行存，重排用） ---------- */

    /// 某 mermaid 围栏块的缩放（缺省 1.0）。
    pub(crate) fn mermaid_zoom(&self, src_line: usize) -> f32 {
        self.rd
            .borrow()
            .mermaid_zoom
            .borrow()
            .get(&src_line)
            .copied()
            .unwrap_or(1.0)
    }

    /// 设置某围栏块缩放并自增缩放代（纳入帧缓存/基础布局代 → 整帧+块高按新比例重建）。
    pub(crate) fn set_mermaid_zoom(&self, src_line: usize, z: f32) {
        self.rd.borrow_mut().mermaid_zoom.borrow_mut().insert(src_line, z);
        let rd = self.rd.borrow_mut();
        rd.mermaid_zoom_gen.set(rd.mermaid_zoom_gen.get().wrapping_add(1));
    }

    /// mermaid 缩放代数（进帧缓存键 + 基础布局代）。
    pub(crate) fn mermaid_zoom_gen(&self) -> u64 {
        self.rd.borrow().mermaid_zoom_gen.get()
    }

    /// 当前获得焦点的 mermaid 卡（`src_line`）；`None` = 无（滚轮在 mermaid 上将继续滚动）。
    pub(crate) fn focused_mermaid(&self) -> Option<usize> {
        self.focused_mermaid
    }

    /* ---------- 代码块高亮：后台 syntect 整块解析（把 debug 下 ~300ms/块移出主线程） ---------- */

    /// 查代码块高亮解析缓存：就绪返回高亮 run，否则按 NotReady 处理。命中即 touch
    /// （最近使用：shape 期对带内代码块调用，LRU 以此判定淘汰面）。
    pub(crate) fn code_parse_lookup(&self, lang: &str, code: &str) -> CodeParseLookup {
        let mut m = self.code_parse.lock().unwrap();
        let key: CodeParseKey = (lang.into(), code.into());
        match m.lookup(&key) {
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
                // 满容量且键不在场 → LRU 淘汰最久未用（runs 是纯 CPU 数据，直接弃）。
                let _ = m.insert(key.clone(), CodeParseState::Pending);
            }
        }
        // 记录**每个**请求块的 src_line：同一内容键可被多个内容完全相同的代码块请求
        // （sync_code_parse 按键分组、就绪时对全部同内容块定向作废缓存，否则滚回带内
        // 第二个相同副本因 request 去重永不重上色、永久保持纯文本）。
        let mut need = self.code_parse_need.borrow_mut();
        if !need.iter().any(|(k, s)| k == &key && *s == src_line) {
            need.push((key, src_line));
        }
        // 捕获请求时刻该块的**内容指纹**（`block_content_id`：kind+跨度+首末行文本）——
        // 不随块下标/行号位移。`on_code_parse_ready` 完成回调按此钉作废整形缓存，替代
        // 「当刻 `src_line → block`」反查：增量编辑行平移后旧 src_line 反查会作废同一
        // 位置的无关块、漏作废真正的内容块（P1-7 编辑错位）。同块重复请求以最新一次
        // 内容为准（覆盖钉即可）。
        let cid = frame::block_content_id(&self.doc, self.doc.block_index_for_line(src_line));
        self.code_parse_pins.borrow_mut().insert(src_line, cid);
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
            // 换源守卫代号：request 后若 `install_open` 换过源，本次解析结果即属旧文档。
            let claim_epoch = self.code_epoch.get();
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
                // 换源守卫：跨了 `install_open`（epoch 变）的旧任务不写新文档缓存、也
                // 不按旧 `src_line` 作废新文档的块（否则跨源斜错位作废）。
                let fresh = this
                    .update(cx, |e, _| e.code_epoch.get() == claim_epoch)
                    .unwrap_or(false);
                if !fresh {
                    return;
                }
                {
                    let mut m = cache.lock().unwrap();
                    let _ = m.insert((lang.clone(), code.clone()), CodeParseState::Done(result));
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
        // 用「请求时刻捕获的块内容指纹」作废整形缓存，不用当刻 `src_line → block` 反查：
        // 增量编辑行平移后旧 src_line 反查会作废同一位置的无关块、漏作废真正的内容块
        // （P1-7 编辑错位）。指纹不随块下标/行号位移；块内容已变则旧指纹自然不匹配、
        // 作废无害（新内容早已按新指纹另排）。未登记钉（测试直接模拟就绪 / 换源后
        // 残留）回退当刻反查兜底——内容未变的块指纹等价，作废目标一致。
        let mut used: Vec<(usize, u64)> = Vec::new();
        let mut ids: Vec<u64> = Vec::new();
        {
            let pins = self.code_parse_pins.borrow();
            for &s in src_lines {
                match pins.get(&s) {
                    Some(&cid) => {
                        ids.push(cid);
                        used.push((s, cid));
                    }
                    None => ids.push(frame::block_content_id(
                        &self.doc,
                        self.doc.block_index_for_line(s),
                    )),
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        self.rd
            .borrow_mut()
            .shape_rows
            .borrow_mut()
            .invalidate_ids(&ids);
        let in_band = self
            .rd
            .borrow()
            .frame
            .borrow()
            .as_ref()
            .is_some_and(|f| {
                f.rows.iter().any(|r| {
                    let cid = frame::block_content_id(
                        &self.doc,
                        self.doc.block_index_for_line(r.src_line),
                    );
                    ids.contains(&cid)
                })
            });
        if in_band {
            *self.rd.borrow_mut().frame.borrow_mut() = None;
            cx.notify();
        }
        // 消费钉（一次性）：仅当钉仍等于本次所用指纹才删（期间同 src_line 有新请求
        // 覆盖为更新指纹 → 留给新任务的完成回调用）。
        {
            let mut pins = self.code_parse_pins.borrow_mut();
            for (s, cid) in used {
                if pins.get(&s) == Some(&cid) {
                    pins.remove(&s);
                }
            }
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
        // 换源（`install_open`）清 `math_slots` 收拢的旧文档公式位图纹理：本帧统一
        // `drop_image` 释放（install_open 无 cx，释放延迟到每帧这里；Arc 未 drop 前
        // 字节仍在、可重传，不丢渲染）。
        let pending = std::mem::take(&mut *self.pending_math_drop.borrow_mut());
        for img in pending {
            cx.drop_image(img, None);
        }
        let color = math_color(&self.rd.borrow().theme);
        let color_changed = self.math_color.borrow().as_ref() != Some(&color);
        if color_changed {
            self.math_color.borrow_mut().replace(color);
            // 主题色变化：旧位图颜色失效 → 槽与钉一并清空重渲；位图纹理同步
            // `drop_image` 释放（旧实现只 clear 槽、纹理留在 sprite atlas 只涨不降，
            // 反复切主题累积内存）。
            let old = self.math_slots.borrow_mut().drain_all();
            for s in old {
                if let MathSlot::Ready(img, _, _) = s {
                    cx.drop_image(img, None);
                }
            }
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
            // 行内公式登记「所在行 + 块内容指纹」→ 完成时按指纹定向作废该行整形缓存
            // （见 `inline_math_rows` 注释）。指纹在注册时刻捕获、随行平移稳定——
            // `on_math_ready` 不再用当刻 `src_line → block` 反查（P1-7）。
            if is_inline {
                let mut rows = self.inline_math_rows.borrow_mut();
                let v = rows.entry(key.clone()).or_default();
                if !v.iter().any(|(s, _)| *s == src_line) {
                    let cid =
                        frame::block_content_id(&self.doc, self.doc.block_index_for_line(src_line));
                    v.push((src_line, cid));
                }
                drop(rows);
            }
            let mut slots = self.math_slots.borrow_mut();
            // 带内公式统一 touch：带内 = 最近使用，LRU 不赶在途/正在显示的公式
            // （Ready/Loading/Failed 一致；滚出带外久未回来的公式不再被 touch）。
            slots.touch(key.as_ref());
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
            // claim Loading：容量满且键不在场 → LRU 淘汰最久未用条目，其 Ready 位图
            // 纹理当场 `drop_image` 释放（防纹理只涨不降）。
            let write = slots.insert(key.clone(), MathSlot::Loading);
            if let Some(MathSlot::Ready(img, _, _)) = write.evicted {
                cx.drop_image(img, None);
            }
            drop(slots);
            // 离屏排版（纯 CPU；与 zorite `ensure_math_loaded` 同构）。
            let slots = self.math_slots.clone();
            let color2 = color;
            // 换源守卫代号：claim 后若 `install_open` 换过源，本次光栅结果即属旧文档。
            let claim_epoch = self.math_epoch.get();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        // 行内先试 `\textstyle …`（大算子转上下标、只压算子）；包层解析
                        // 失败（如某些环境命令）则回退原样排版，避免把好公式降级成 Unicode。
                        if is_inline {
                            let ts = format!("\\textstyle {latex}");
                            markdown_wysiwyg_math::render_latex_with_pad(
                                &ts,
                                config::math::MATH_FONT_SIZE,
                                config::math::MATH_DPR,
                                color2,
                                pad,
                            )
                            .or_else(|| {
                                markdown_wysiwyg_math::render_latex_with_pad(
                                    &latex,
                                    config::math::MATH_FONT_SIZE,
                                    config::math::MATH_DPR,
                                    color2,
                                    pad,
                                )
                            })
                        } else {
                            markdown_wysiwyg_math::render_latex_with_pad(
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
                // 换源守卫：跨了 `install_open`（epoch 变）的旧任务不写新文档槽、不触发
                // 就绪——新文档会自行 claim/重排；其 Ready 位图纹理登记待释放。
                let fresh = this
                    .update(cx, |e, _| e.math_epoch.get() == claim_epoch)
                    .unwrap_or(false);
                if !fresh {
                    if let MathSlot::Ready(img, _, _) = &slot {
                        let _ = this.update(cx, |e, _| e.queue_math_drop(img.clone()));
                    }
                    return;
                }
                let slots2 = slots.clone();
                let key2 = key.clone();
                let became_ready = {
                    let mut slots2 = slots2.borrow_mut();
                    // 占位 `Loading` 已被 LRU 淘汰 / 换源清空（键不在场）→ 不复活已淘汰
                    // 槽：结果弃置（旧任务不写入已淘汰槽），其 Ready 位图纹理登记待释放。
                    if !matches!(slots2.get(key2.as_ref()), Some(MathSlot::Loading)) {
                        if let MathSlot::Ready(img, _, _) = &slot {
                            let _ = this.update(cx, |e, _| e.queue_math_drop(img.clone()));
                        }
                        return;
                    }
                    // 完成时自增 `math_gen`（on_math_ready）使帧/折叠计划按**真实位图高**
                    // 重建：这里 key 必已以 `Loading` 占位 → `old = Some(Loading)` 即判定
                    // “排版完成”（旧的 `.is_none()` 恒假——占位符永远在场，mgen 从不自增
                    // → 卡片永远停在回退高 42px，68px 位图越出灰底——本 bug 自 RaTeX 接入
                    // 起即存在）。
                    slots2
                        .insert(key2.clone(), slot)
                        .old
                        .is_some_and(|old| matches!(old, MathSlot::Loading))
                };
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
            // 用「claim 时捕获的块内容指纹」作废（P1-7，见 `inline_math_rows` 注释）——
            // 不用当刻 `src_line → block` 反查：增量编辑行平移后旧 src_line 反查会作废
            // 同一位置的无关块、漏作废真正含该公式的块。带内行再按其**内容**（键相等）
            // 找到当前帧中仍含该公式的块补钉（行平移后注册时 src_line 已旧，但帧内行
            // 按内容匹配帧行块指纹仍精确覆盖）。
            let mut pinned: Vec<(usize, u64)> = Vec::new();
            if let Some(v) = self.inline_math_rows.borrow().get(key) {
                pinned.extend(v.iter().copied());
            }
            if let Some(f) = self.rd.borrow().frame.borrow().as_ref() {
                for r in &f.rows {
                    if r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key) {
                        let cid = frame::block_content_id(
                            &self.doc,
                            self.doc.block_index_for_line(r.src_line),
                        );
                        pinned.push((r.src_line, cid));
                    }
                }
            }
            if !pinned.is_empty() {
                let mut ids: Vec<u64> = pinned.iter().map(|&(_, c)| c).collect();
                ids.sort_unstable();
                ids.dedup();
                self.rd
                    .borrow_mut()
                    .shape_rows
                    .borrow_mut()
                    .invalidate_ids(&ids);
            }
        }
        let in_band = self.rd.borrow().frame.borrow().as_ref().is_some_and(|f| {
            f.rows.iter().any(|r| {
                if let Some(element::BlockCard::Math { latex, .. }) = &r.extra.card
                    && latex.trim() == key
                {
                    return true;
                }
                r.extra.inline_maths.iter().any(|m| m.key.as_ref() == key)
            })
        });
        self.rd.borrow().math_gen.set(self.rd.borrow().math_gen.get().wrapping_add(1));
        if in_band {
            *self.rd.borrow_mut().frame.borrow_mut() = None;
            cx.notify();
        }
    }
}
