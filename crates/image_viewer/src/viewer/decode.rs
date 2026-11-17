//! 解码（decode）子模块：并行解码池（去重 / 并发槽 / 追最新）、解码结果与字节 LRU 缓存、
//! 方向感知的级联预取、动图后台时钟，以及 EXIF 嵌入缩略图「即时首帧」占位。
//!
//! 低层解码原语（JPEG/PNG/GIF/EXIF 字节处理、盒式降采样等自由函数）留在 [`super`]（根文件，
//! 供 `mod tests` 经 `use super::*` 直取）；本子模块只做「流编排」状态机，全部经
//! `impl super::ImageViewer` 挂到类型上。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, RenderImage};
use kimg::Frame;

use config::image::{
    CACHE_MAX_BYTES, CACHE_MAX_ITEMS, FAST_SCROLL_MAX_DECODE, HEAVY_DECODE_MAX, MAX_DECODE,
    PREFETCH_BACKWARD, PREFETCH_FORWARD, PREFETCH_FORWARD_FAST,
};

use super::{
    bgra_single_frame, decode_background, decode_jpeg_display, extract_exif_jpeg_thumbnail,
    is_heavy_format, path_is_jpeg, pick_den, read_head, ByteEntry, CacheEntry, DISPLAY_NEED_INIT,
};

/// EXIF APP1 探读上限（字节）：EXIF 段及其内嵌缩略图都位于文件头（APP1 长度 16 位、
/// ≤64KB），读头即可提取，不必全量读盘。
const EXIF_PROBE_BYTES: usize = 256 * 1024;

/// 动图单帧**最小显示间隔**（ms）：GIF 时钟不得以短于此间隔推进——GIF 帧 delay=0ms
///（无 GCE 时解析器默认 0ms）若按 `timer(0)` 立即就绪会被瞬间跳过，形成 60fps 级
/// 重绘风暴。任何帧延迟（含 0ms）都先夹到该下界再计时（[`Self::anim_wait`]）。
const ANIM_FRAME_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

impl super::ImageViewer {

    /// P1b：EXIF 嵌入缩略图「即时首帧」（零依赖；school 等手机照片在 APP1-Exif 的 IFD1 常嵌
    /// 一张 ~160px 缩略 JPEG）。冷目标且是 JPEG 时派发：后台只读**文件头**（EXIF 段及其内嵌
    /// 缩略图都位于头部，APP1 长度 16 位 ≤64KB，读 256KB 足够）→ 提取 → 解码 → 装成**临时
    /// 占位**上屏。占位**不**升档（`display_denom` 置 1 短路 [`Self::escalate_display_denom`]）、
    /// **不**重锚级联预取、**不**进 LRU——这些等真图 [`Self::install_current`] 再做（打开首图
    /// 仍独占解码槽不抢跑）。`loading` 保持 true：真图未就绪前才够资格占位。
    pub(super) fn maybe_show_exif_thumb(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !path_is_jpeg(path) || self.thumb_pending.as_deref() == Some(path) {
            return;
        }
        let cached = self.byte_cache_get(path);
        self.thumb_pending = Some(path.to_path_buf());
        let target = path.to_path_buf();
        let probe = target.clone();
        cx.spawn(async move |weak, cx| {
            let thumb = cx
                .background_executor()
                .spawn(async move {
                    let head = match cached {
                        Some(b) => Some(b[..b.len().min(EXIF_PROBE_BYTES)].to_vec()),
                        None => read_head(&probe, EXIF_PROBE_BYTES),
                    };
                    head.and_then(|h| extract_exif_jpeg_thumbnail(&h))
                        .and_then(|tb| decode_jpeg_display(&tb, DISPLAY_NEED_INIT).ok())
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.thumb_pending.as_deref() == Some(target.as_path()) {
                    this.thumb_pending = None;
                }
                if let Some((bgra, w, h, den)) = thumb
                    && this.path == target
                    && this.state.is_loading()
                    && let Some(frame) = bgra_single_frame(bgra, w, h)
                    && let Some(entry) =
                        Self::build_entry(target.clone(), vec![frame], w, h, den)
                {
                    this.install_provisional(entry, cx);
                }
            });
        })
        .detach();
    }

    /// 把临时占位（EXIF 缩略图）上屏：只替换 `image`/`intrinsic`，并置 `display_denom=1`
    /// （临时小图不触发升档）。不动 `loading`、不重锚预取、不进 LRU——真图
    /// [`Self::install_current`] 会覆盖这一切。
    fn install_provisional(&mut self, entry: CacheEntry, cx: &mut Context<Self>) {
        self.state.data = Some(entry.render);
        self.intrinsic = Some(entry.intrinsic);
        self.display_denom = 1;
        cx.notify();
        cx.refresh_windows();
    }

    /// **快滚节流闸**：当前「有效解码并发上限」——快速连续滚动（`scroll_fast`）时收敛到
    /// [`FAST_SCROLL_MAX_DECODE`]（快滚多张猛甩，8 路并发把核打满是 CPU 放大器，且大量
    /// 预取未被看），否则回 [`MAX_DECODE`]。动态上限**统一收口**到本方法：目标解码
    /// （[`Self::queue_decode`] / 升档 / [`Self::drain_pending`] 追最新）与预取
    /// （[`Self::pump_prefetch`] 的在途判定）全部经它取值，避免散点常量。重型格式仍由
    /// [`HEAVY_DECODE_MAX`] 独立封顶（=1 ≤ 快滚闸），取 min 不顶破。
    fn max_decode(&self) -> usize {
        if self.scroll_fast {
            FAST_SCROLL_MAX_DECODE
        } else {
            MAX_DECODE
        }
    }

    /// 解码池入队：已在解 → 等其落盘追最新；已缓存 → 直接落盘；并发满（[`Self::max_decode`]）
    /// → 仅**显示目标**（`is_target`）记入 [`pending_target`] 等空位（预取在满时直接放弃）；
    /// 否则派发并行解码。`in_flight` 去重。**重型格式**（HEIF）在途数 ≥
    /// [`HEAVY_DECODE_MAX`] 时同款：目标记 [pending_target]、预取放弃（见
    /// [`Self::spawn_decode`] 内的重型闸）。
    pub(super) fn queue_decode(&mut self, path: PathBuf, is_target: bool, cx: &mut Context<Self>) {
        if self.in_flight.contains(&path) {
            return;
        }
        if let Some(entry) = self.cache_get(&path) {
            if is_target && path == self.path {
                self.install_current(entry, &path, cx);
            }
            return;
        }
        if self.in_flight.len() >= self.max_decode() {
            if is_target {
                self.pending_target = Some(path);
            }
            return;
        }
        self.spawn_decode(path, !is_target, cx);
    }

    /// 判定路径是否为**重型格式**（HEIF/HEIC，受 [`HEAVY_DECODE_MAX`] 并发上限约束）：
    /// 优先用调用方已取的缓存字节（零 IO）做字节判定；没有时用字节缓存，再没有才读
    /// **文件头部**（`ftyp` box 在文件首 64 字节内）——以字节为准、零全量读盘。扩展名
    /// 非 heif 系直接放行（零 IO 快速路径；伪装成其它扩展名的 HEIF 在解码层仍按
    /// [`decode_background`] 的字节分支降档，只是不享受并发上限——可接受的口径差）。
    fn path_is_heavy(&mut self, path: &Path, have: Option<&[u8]>) -> bool {
        let ext_heif = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "heic" | "heif" | "hif"));
        if !ext_heif {
            return false;
        }
        match have {
            Some(bytes) => is_heavy_format(bytes),
            None => {
                if let Some(arc) = self.byte_cache_get(path) {
                    is_heavy_format(&arc)
                } else {
                    read_head(path, 64).is_some_and(|h| is_heavy_format(&h))
                }
            }
        }
    }

    /// 派发一次并行解码（解码池槽位之一）：优先用字节缓存里的内存字节，否则后台读盘并把
    /// 字节回填缓存（重解码不重读盘）。按当前「展示像素需要」解码（JPEG/PNG/HEIF 走自研
    /// kimg 全解 + 盒式降档直出 BGRA；其它格式走 `image` 全解）。结果只进缓存；仅当「仍是
    /// 当前显示目标」才上屏。
    ///
    /// **重型格式并发闸**（[`HEAVY_DECODE_MAX`]）：HEIF 全解秒级 + 数十 MB RGBA，8 路并发
    /// 会把核长期打满→ 在途重型数 ≥ 上限时，目标记 [`pending_target`] 等空位、预取直接
    /// 放弃（不占解码槽）；JPEG/PNG/GIF 不受此限。重型计数在**派发前**自增、完成后自减
    /// （配对 1:1，被闸拒绝的请求不计数）。
    fn spawn_decode(&mut self, path: PathBuf, is_prefetch: bool, cx: &mut Context<Self>) {
        let bytes = self.byte_cache_get(&path);
        let heavy = self.path_is_heavy(&path, bytes.as_ref().map(|b| &b[..]));
        // 先于 in_flight.insert：重型并发闸拒绝时不产生在途、不占槽位
        if heavy && self.heavy_inflight >= HEAVY_DECODE_MAX {
            if !is_prefetch {
                self.pending_target = Some(path);
            }
            return;
        }
        self.in_flight.insert(path.clone());
        if heavy {
            self.heavy_inflight += 1;
        }
        if is_prefetch {
            self.prefetch_inflight += 1;
        }
        let decode_path = path;
        let read_path = decode_path.clone();
        let need = self.display_need;
        cx.spawn(async move |weak, cx| {
            // 后台：取字节（缓存命中直接用；未中读盘）→ 按展示需要解码 → 返回 (原始字节, 结果)。
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let raw: Arc<Vec<u8>> = match bytes {
                        Some(b) => b,
                        None => Arc::new(std::fs::read(&read_path).map_err(|e| e.to_string())?),
                    };
                    let decoded = decode_background(&raw, need);
                    Ok::<_, String>((raw, decoded))
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.in_flight.remove(&decode_path);
                if is_prefetch {
                    this.prefetch_inflight -= 1;
                }
                if heavy {
                    this.heavy_inflight = this.heavy_inflight.saturating_sub(1);
                }
                match outcome {
                    Ok((raw, decoded)) => {
                        // 回填字节缓存（跳过再回看不重读盘）
                        this.byte_cache_put(
                            decode_path.clone(),
                            ByteEntry { bytes: raw },
                        );
                        match decoded {
                            Ok((frames, w, h, den)) => {
                                if let Some(entry) =
                                    Self::build_entry(decode_path.clone(), frames, w, h, den)
                                {
                                    this.cache_put(decode_path.clone(), entry.clone(), cx);
                                    if this.path == decode_path {
                                        this.install_current(entry, &decode_path, cx);
                                    }
                                } else if this.path == decode_path {
                                    this.state.set_error("解码结果尺寸异常");
                                    cx.notify();
                                    cx.refresh_windows();
                                }
                            }
                            Err(e) => {
                                if this.path == decode_path {
                                    this.state.set_error(e);
                                    cx.notify();
                                    cx.refresh_windows();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if this.path == decode_path {
                            this.state.set_error(e);
                            cx.notify();
                            cx.refresh_windows();
                        }
                    }
                }
                // 并发槽空出 → 追最新未派发目标，并顺势把预取窗口再往前推一格
                this.drain_pending(cx);
                this.pump_prefetch(cx);
            });
        })
        .detach();
    }

    /// 并发槽空出后的兜底：把「最新显示目标」重新入队（已在解 / 已缓存的情况下
    /// [`Self::queue_decode`] 自行处理；重型闸挂起的目标同样在此放出：重型计数已自减、
    /// 重入队列即通过闸）。
    pub(super) fn drain_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.pending_target.take() {
            self.queue_decode(p, true, cx);
        }
    }

    /// 「升档」：当前展示档分母比当前展示需要更粗（解码像素比显示小）时，重解更细档替换
    /// （JPEG/PNG/通用静态皆可——选档 [`pick_den`] 与格式无关）。直接派发（跳过
    /// [`Self::queue_decode`] 的缓存短路——缓存里存的旧档位命中只会装回旧图）；
    /// `in_flight`/并发满则记 [`Self::pending_target`]。完成后新档经 `cache_put` 替换
    /// 粗档并上屏，`display_denom` 收敛到 ≤ 需要 → 不再触发（只升不降）。
    pub(super) fn escalate_display_denom(&mut self, cx: &mut Context<Self>) {
        // display_denom==1 已满档（全解）不升；去掉 `path_is_jpeg` 限定让 PNG den>1 条目
        // 在 display_need 变大（放大/窗口变大）时同样能重解更细档
        if self.display_denom <= 1 {
            return;
        }
        let Some((iw, ih)) = self.intrinsic else {
            return;
        };
        // `intrinsic` 是**降档后**的尺寸：选档基准必须按「全量原图」估回（×display_denom），
        // 否则在「当前降档输出已 ≥ 展示需要」区会算出 desired=1 < display_denom →
        // 每次缩放/回看缓存图都触一次重解，又落回同一分母（浪费一次全量 PNG 解码）。
        let full = (
            (iw as u32).saturating_mul(u32::from(self.display_denom)),
            (ih as u32).saturating_mul(u32::from(self.display_denom)),
        );
        let desired = pick_den(full.0, full.1, self.display_need.0, self.display_need.1);
        if self.display_denom <= desired {
            return;
        }
        let path = self.path.clone();
        if self.in_flight.contains(&path) {
            return;
        }
        if self.in_flight.len() >= self.max_decode() {
            self.pending_target = Some(path);
            return;
        }
        self.spawn_decode(path, false, cx);
    }

    /// 从 BGRA 帧序列构建缓存条目（`denom` 记入条目：展示档位 / 升档判定用）。帧序列
    /// 必须横向统一尺寸（`w×h`）——GIF 解码已按逻辑屏画布统一；`bytes` 按
    /// 「单帧字节 × 帧数」诚实记账：大 GIF 会如实算成块头、超 [`config::image::CACHE_MAX_BYTES`]
    /// 预算时被 LRU 立刻淘汰——可接受（动画帧必须全量驻留才能播，口径如实反映）。
    pub(super) fn build_entry(
        path: PathBuf,
        frames: Vec<Frame>,
        w: u32,
        h: u32,
        denom: u8,
    ) -> Option<CacheEntry> {
        if frames.is_empty() {
            return None;
        }
        let bytes = (w as usize * h as usize * 4).checked_mul(frames.len())?;
        // 逐帧切片成**独立单帧** RenderImage（详见 [`CacheEntry::gif`]：绕开 gpui `img()`
        // 只在窗口聚焦时自动动画的限制，改由本查看器时钟驱动）。首帧同时作为 `render`
        // （静态预览/占位）；帧 buffer 随 Frame 移入各自 RenderImage，不额外复制内存。
        let mut per_frame: Vec<Arc<RenderImage>> = Vec::with_capacity(frames.len());
        let mut delays: Vec<std::time::Duration> = Vec::with_capacity(frames.len());
        for frame in frames {
            delays.push(std::time::Duration::from(frame.delay()));
            per_frame.push(Arc::new(RenderImage::new(vec![frame])));
        }
        let render = per_frame[0].clone();
        Some(CacheEntry {
            path,
            render,
            intrinsic: (w as f32, h as f32),
            bytes,
            denom,
            gif: (per_frame.len() > 1).then(|| (per_frame, delays)),
        })
    }

    /// 方向感知的**双向级联预取**（滑动窗口）：
    /// 1. **主方向**（最近的滚动方向）：随后 `PREFETCH_FORWARD`（快速滚动放宽到
    ///    [`PREFETCH_FORWARD_FAST`]）张逐个排队解码，窗口按到当前图的距离封口——绝不超前
    ///    解到窗口外（否则会把用户正要滚到的图从 LRU 挤掉）；
    /// 2. **反向**（刚看过的往回侧）：非快速滚动时保留 [`PREFETCH_BACKWARD`] 张，回看上一张/
    ///    回头翻不因 LRU 淘汰变冷；快速滚动时让位给前方深预取；
    /// 3. 解码槽全满时给窗口外最近几张预读字节（IO 不占解码槽）。
    /// 4. 预取统一封顶 `[Self::max_decode] - 1` 个在途解码（预留 1 槽给「冷目标」秒占；快速
    ///    滚动时受快滚节流闸 [`FAST_SCROLL_MAX_DECODE`] 约束，不再 8 并发深挖 12 张）；
    ///    显示目标在等槽位优先、窗口铺满、或游标越过锚点一整圈（全清单已覆盖）时自停。
    fn pump_prefetch(&mut self, cx: &mut Context<Self>) {
        if self.pending_target.is_some() {
            return;
        }
        let n = self.nav.len() as i64;
        if n <= 1 {
            return;
        }
        // B4：快速滚动 → 放宽前向窗口
        let forward_win = if self.scroll_fast {
            PREFETCH_FORWARD_FAST
        } else {
            PREFETCH_FORWARD
        } as i64;
        // 1) 主方向窗口：dist 1..=forward_win（预取封顶 max_decode-1，预留冷目标槽）
        while (self.prefetch_pos - self.prefetch_origin).abs() < n
            && self.prefetch_inflight < self.max_decode().saturating_sub(1)
            && (self.prefetch_pos - self.prefetch_origin).abs() <= forward_win
        {
            // 解码缓存已满（字节预算）→ 预取一解即被 LRU 挤掉，纯白算 CPU：停止新派发
            if self.cache.bytes() >= CACHE_MAX_BYTES {
                break;
            }
            let ni = self.prefetch_pos.rem_euclid(n) as usize;
            let Some(path) = self.nav.item(ni) else {
                break;
            };
            self.prefetch_pos += self.prefetch_dir as i64;
            if self.cache_contains(&path) || self.in_flight.contains(&path) {
                continue;
            }
            self.queue_decode(path, false, cx);
            if self.pending_target.is_some() {
                return;
            }
        }
        // 2) 反向窗口：非快速滚动时，当前图往回 PREFETCH_BACKWARD 张（低优先，仅余量时补）
        if !self.scroll_fast {
            let base = self.nav.current() as i64;
            for k in 1..=PREFETCH_BACKWARD as i64 {
                if self.prefetch_inflight + 1 >= self.max_decode()
                    || self.cache.len() >= CACHE_MAX_ITEMS
                    || self.cache.bytes() >= CACHE_MAX_BYTES
                {
                    break;
                }
                let ni = (base - k).rem_euclid(n) as usize;
                let Some(path) = self.nav.item(ni) else {
                    break;
                };
                if self.cache_contains(&path) || self.in_flight.contains(&path) {
                    continue;
                }
                self.queue_decode(path, false, cx);
                if self.pending_target.is_some() {
                    return;
                }
            }
        }
        // 3) 解码槽全满：给窗口后最近几张预读字节（IO 不占解码槽；等空位时不再等磁盘；
        //    门槛随 max_decode 走：快滚态解码并发小，槽一满即预读，IO 与解码分离不互占）
        if self.prefetch_inflight + 1 >= self.max_decode() {
            for _ in 0..self.max_decode() {
                if (self.prefetch_pos - self.prefetch_origin).abs() >= n {
                    break;
                }
                let ni = self.prefetch_pos.rem_euclid(n) as usize;
                let Some(path) = self.nav.item(ni) else {
                    break;
                };
                self.prefetch_pos += self.prefetch_dir as i64;
                self.preload_bytes(path, cx);
            }
        }
    }

    /// 磁盘→RAM 字节预读：把相邻文件压缩字节读进 LRU 缓存（IO 与解码分离，便宜便宜）。
    /// 去重：已在字节缓存 / 字节预读在途 / 正在解码（解码自身会读盘回填）则跳过。
    fn preload_bytes(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.byte_cache_contains(&path)
            || self.byte_flight.contains(&path)
            || self.in_flight.contains(&path)
        {
            return;
        }
        self.byte_flight.insert(path.clone());
        let p = path;
        let p_inner = p.clone();
        cx.spawn(async move |weak, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&p_inner).ok().map(Arc::new) })
                .await;
            let _ = weak.update(cx, |this, _cx| {
                this.byte_flight.remove(&p);
                if let Some(b) = read
                    && !this.byte_cache_contains(&p)
                {
                    this.byte_cache_put(p.clone(), ByteEntry { bytes: b });
                }
            });
        })
        .detach();
    }

    /// 把缓存条目落盘为当前显示 + 通知宿主 + 重锚级联预取（窗口沿方向滑动更新）。
    pub(super) fn install_current(&mut self, entry: CacheEntry, path: &Path, cx: &mut Context<Self>) {
        // 诊断探针（env 门控，默认关）：记录落盘时刻 t1，结算 decode_ms=t1-t0
        super::diag::note_install(path);
        // R4A 边界「当前显示与目标相同不闪」：同路径且同渲染 Arc 的重复落盘 = 重画同一张
        // 图，整段状态（path/image/intrinsic/denom/loading/动画/预取锚）已就位——直接早退，
        // 不再 notify + refresh，省下一次「全树重述 + present」周期。
        if self.path == *path
            && self.state.data.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &entry.render))
        {
            return;
        }
        // P1-29 资源回收：替换显示前先记下「旧显示渲染 + 旧动图片段」——替换后它们脱离
        // `state.data`/`anim_clip`；若已不在缓存（升级重解顶掉旧档 / 逐出-翻页循环时被 LRU
        // 逐出、但因「当前显示」在 `drop_evicted_images` 里被 guard 跳过），则显式
        // `drop_image` 下线 GPU 纹理，防 VRAM 图集孤儿累积（尤其 50MP 全解档）。仍在缓存
        // → 由 LRU 淘汰 / `release` 兜底，绝不提前释放。
        let old_display = self.state.data.clone();
        let old_clip = self.anim_clip.clone();
        self.path = path.to_path_buf();
        self.state.set_ready(entry.render.clone());
        self.intrinsic = Some(entry.intrinsic);
        self.display_denom = entry.denom;
        // 动图：挂上帧切片并启动后台时钟（详见 [`Self::spawn_anim_clock`]；reduce_motion
        // 时保持静态首帧，尊重「减少动态效果」设置）。换图/重装必先递增代序号自停旧时钟。
        self.anim_gen = self.anim_gen.wrapping_add(1);
        let anim = entry
            .gif
            .as_ref()
            .is_some_and(|(f, _)| f.len() > 1)
            && !cx.reduce_motion();
        self.anim_clip = if anim { entry.gif.clone() } else { None };
        self.anim_index = 0;
        // P1-29：释放「已不在缓存中」的旧显示渲染 / 旧动图片段（去重：动图首帧与显示
        // 是同一 Arc；`retire_render` 内再对缓存持有做一次守卫过滤）。
        let mut to_retire: Vec<Arc<RenderImage>> = Vec::new();
        if let Some(old) = old_display {
            to_retire.push(old);
        }
        for f in old_clip.iter().flat_map(|(frames, _)| frames.iter()) {
            if !to_retire.iter().any(|x| Arc::ptr_eq(x, f)) {
                to_retire.push(f.clone());
            }
        }
        for r in to_retire {
            self.retire_render(&r, cx);
        }
        self.update_display_need();
        cx.notify();
        cx.refresh_windows();
        if anim {
            self.spawn_anim_clock(cx);
        }
        // 锚点移到当前位置：前向/反向窗口随位置滑动，继续沿方向补满
        self.prefetch_origin = self.nav.current() as i64;
        self.prefetch_pos = self.nav.current() as i64 + self.prefetch_dir as i64;
        self.pump_prefetch(cx);
    }

    /// 动图时钟帧等待：对帧 `delay` 施加**最小显示间隔下界**（`max`，只抬不压）。
    /// GIF 帧 delay 无上界语义、但可有 0ms（无 GCE 时解析器默认 0ms）——若
    /// `timer(0)` 立即就绪，0ms 帧会被瞬间跳过，形成 60fps 级重绘风暴（每帧白白
    /// notify+refresh 整窗）。任何帧延迟（含 0ms）都先夹到 [`ANIM_FRAME_MIN_INTERVAL`]
    /// 再计时；正常 ≥20ms 的帧延迟原样走。
    fn anim_wait(delay: std::time::Duration) -> std::time::Duration {
        delay.max(ANIM_FRAME_MIN_INTERVAL)
    }

    /// 后台动画时钟：按各帧时长逐帧推进当前动图（`self.state.data` = 当前帧单帧
    /// `RenderImage`），并 `notify`+`refresh_windows` 触发重绘——由本查看器驱动，**不依赖
    /// 窗口焦点**（gpui `img()` 自动动画只在窗口聚焦时推进，失焦即冻结，故绕开它）。
    /// 以 `anim_gen` 代序号防旧任务误更新：换图/重装后 `install_current` 递增代序号，
    /// 旧任务在下一轮更新时发现不匹配即自停；实体销毁后 `update` 失败自停。
    fn spawn_anim_clock(&mut self, cx: &mut Context<Self>) {
        let clock_gen = self.anim_gen;
        let Some((frames, delays)) = self.anim_clip.clone() else {
            return;
        };
        let n = frames.len();
        if n <= 1 {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut idx = 0usize;
            loop {
                let delay = delays
                    .get(idx)
                    .copied()
                    .unwrap_or(std::time::Duration::from_millis(100));
                let wait = Self::anim_wait(delay);
                cx.background_executor().timer(wait).await;
                let alive = this.update(cx, |this, cx| {
                    if this.anim_gen != clock_gen {
                        return false;
                    }
                    idx = (idx + 1) % n;
                    this.anim_index = idx;
                    this.state.data = Some(frames[idx].clone());
                    cx.notify();
                    cx.refresh_windows();
                    true
                });
                if alive.map_or(true, |v| !v) {
                    return; // 换图 / 实体销毁 → 自停
                }
            }
        })
        .detach();
    }

    /// 缓存查询并移到队尾（MRU）。命中返回克隆（Arc 浅拷贝），未中 `None`。
    pub(super) fn cache_get(&mut self, path: &Path) -> Option<CacheEntry> {
        self.cache.get(path)
    }

    /// 缓存是否已含某路径（只读，不改变 LRU 顺序）。
    pub(super) fn cache_contains(&self, path: &Path) -> bool {
        self.cache.contains(path)
    }

    /// 写入缓存（已存在则移到队尾），超出张数/字节预算按 LRU 从队首淘汰。
    ///
    /// R4C（资源占用）：被逐出且**非当前显示**的条目，立即 `drop_image` 释放其 GPU 纹理
    /// （首帧 render + 动图各帧），不让 LRU 被顶落后的旧纹理继续占 VRAM/图集；当前正在
    /// 显示的图（`path == self.path`）一律保留，绝不释放。本方法只在后台解码完成回调
    /// （普通实体更新）里调用，非窗口回调内，"当前窗口移出 App.windows"的 K14 场景不成立，
    /// `drop_image(_, None)` 遍历全部窗口即兜住。
    pub(super) fn cache_put(&mut self, path: PathBuf, entry: CacheEntry, cx: &mut Context<Self>) {
        let removed = self.cache.put(path, entry);
        self.drop_evicted_images(cx, removed);
    }

    /// 释放被 LRU 逐出的 GPU 纹理（R4C）：`evicted` 为 `put` 返回的被淘汰/被替换条目。
    /// 跳过当前显示的路径（`self.path`）——当前正在看的图绝不 drop_image；其余条目：
    /// 静态图释放首帧 `render`，动图释放全部帧切片（`render` 即 `gif` 首帧的同一 Arc，
    /// 走帧切片分支即可全覆盖，不重复 drop）。
    fn drop_evicted_images(&mut self, cx: &mut Context<Self>, evicted: Vec<(PathBuf, CacheEntry)>) {
        for (path, entry) in evicted {
            if path == self.path {
                continue;
            }
            match entry.gif {
                Some((frames, _)) => {
                    for f in frames {
                        cx.drop_image(f, None);
                    }
                }
                None => cx.drop_image(entry.render, None),
            }
        }
    }

    /// 缓存是否仍引用某渲染（静态 `render` 或 GIF 帧切片）：仍在缓存 → 回看/重现依赖它、
    /// 绝不能提前 `drop_image`；已不在缓存 → 可安全下线 GPU 纹理。
    fn cache_holds_render(&self, render: &Arc<RenderImage>) -> bool {
        self.cache.iter().any(|e| {
            Arc::ptr_eq(&e.render, render)
                || e.gif
                    .as_ref()
                    .is_some_and(|(frames, _)| frames.iter().any(|f| Arc::ptr_eq(f, render)))
        })
    }

    /// 下线「已不在缓存中的旧显示渲染」的 GPU 纹理（P1-29：显示替换/翻页时旧渲染不再被
    /// 任何缓存条目引用即显式 `drop_image` 回收，防图集孤儿 / VRAM 缓慢累积；对齐 K14）。
    /// `App::drop_image(image, None)` 只遍历 `App.windows` 里**未被正在更新取走**的窗口——
    /// 窗口自身的 key/button 回调（如滚动切图触发的缓存命中替换）当场 drop 会漏掉当前
    /// 窗口，故一律经 `cx.defer` 延迟到本拍更新末尾 flush（同 video `drop_old_frame` /
    /// content_sync `pending_math_drop` 姿势）。仍在缓存中的渲染由调用方先过滤，绝不提前释放。
    fn retire_render(&mut self, old: &Arc<RenderImage>, cx: &mut Context<Self>) {
        if self.cache_holds_render(old) {
            return;
        }
        #[cfg(test)]
        {
            self.test_render_drops += 1;
        }
        let img = old.clone();
        cx.defer(move |app| app.drop_image(img, None));
    }

    /// 字节缓存查询并移到队尾（MRU），返回 Arc 克隆（保留在缓存里供解码/重解码复用）。
    fn byte_cache_get(&mut self, path: &Path) -> Option<Arc<Vec<u8>>> {
        self.byte_cache.get(path).map(|e| e.bytes)
    }

    /// 字节缓存是否已含某路径（只读，不改变 LRU 顺序）。
    fn byte_cache_contains(&self, path: &Path) -> bool {
        self.byte_cache.contains(path)
    }

    /// 写入字节缓存（已存在则移到队尾），超出字节预算 [`BYTE_CACHE_BYTES`] 按 LRU 淘汰。
    fn byte_cache_put(&mut self, path: PathBuf, entry: ByteEntry) {
        let _ = self.byte_cache.put(path, entry);
    }
}
