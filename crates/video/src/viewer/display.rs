use super::*;
impl super::VideoViewer {


    // ---- M6：截图当前帧存 PNG ----

    /// 把当前上屏帧转成 PNG 用的 RGBA 像素（**点截图这次才拷贝**——`self.frame` 的
    /// `RenderImage` 始终持帧 BGRA，`as_bytes` 直接取回、不另存每帧副本）。返回 `None` =
    /// 尚无可截图画面。
    pub(super) fn snapshot_rgba(frame: &gpui::RenderImage) -> Option<(u32, u32, Vec<u8>)> {
        let s = frame.size(0);
        let (w, h) = (s.width.0.max(0) as u32, s.height.0.max(0) as u32);
        if w == 0 || h == 0 {
            return None;
        }
        let mut rgba = frame.as_bytes(0)?.to_vec();
        // BGRA → RGBA（PNG 字节序；上屏字节序 BGRA 同 GPUI RenderImage，见 decode.rs 注释）
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Some((w, h, rgba))
    }


    /// 截图当前帧存 PNG：点截图时才从 `self.frame` 拷贝一份（零每帧开销），BGRA → RGBA
    /// 后写视频同目录（不可写回退系统临时目录）。PNG 编码放后台线程（1080p ~几十 ms，
    /// 不卡主线程播放泵）；完成经 [`Self::show_status`] 右上角浮层提示路径。
    pub(super) fn screenshot_png(&mut self, cx: &mut Context<Self>) {
        if self.gpu_frame.is_some() {
            // 双设备 GPU 直通帧不含 CPU 像素（零回读）——截图像素需回读，属架构性不支持；
            // 用户可切软解后截图。
            self.show_status("GPU 直通帧不含 CPU 像素，无法截图（切软解后可截）".to_string(), cx);
            return;
        }
        let Some(frame) = self.frame.clone() else {
            self.show_status("暂无画面可截图：视频尚未解码出帧".to_string(), cx);
            return;
        };
        let Some((w, h, rgba)) = Self::snapshot_rgba(&frame) else {
            self.show_status("暂无画面可截图：视频尚未解码出帧".to_string(), cx);
            return;
        };
        let path = self.screenshot_path();
        let display = path.display().to_string();
        self.show_status("正在保存截图…".to_string(), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    kimg::save_buffer_with_format(
                        &path,
                        &rgba,
                        w,
                        h,
                        kimg::ExtendedColorType::Rgba8,
                        kimg::ImageFormat::Png,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.show_status(format!("已保存截图：{display}"), cx),
                Err(e) => this.show_status(format!("截图保存失败：{e}"), cx),
            });
        })
        .detach();
    }


    /// 截图落盘路径：视频同目录 `<片名>-截图-<UTC时间戳>.png`；同目录不可写时回退系统
    /// 临时目录（截图像素已按显示尺寸 cap 好，不随 PNG 增大）。
    pub(super) fn screenshot_path(&self) -> PathBuf {
        let name = format!(
            "{}-截图-{}.png",
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "视频".to_string()),
            Self::utc_timestamp()
        );
        let dir = self
            .path
            .parent()
            .filter(|d| d.is_dir())
            .map(|d| d.to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        dir.join(name)
    }


    /// 当前 UTC 时间的 `YYYYMMDD-HHMMSS`（截图文件名用）：Howard Hinnant 民用算法从
    /// Unix 秒直接算日历，不依赖 chrono——唯一且可排序，够截图命名。
    pub(super) fn utc_timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = secs / 86400;
        let rem = secs % 86400;
        let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
    }


    /// 释放被替换下的旧帧图集 tile（只在 `frame` 替换点调用；`None` 直接返回零成本）。
    ///
    /// **必须延迟到本拍更新末尾再 `drop_image`**：gpui `App::drop_image(image, None)` 只遍历
    /// `App.windows` 里**未被正在更新取走**的窗口——窗口自身的 key/button 回调都是在
    /// `update_window_erased`（当前窗口被临时移出 `App.windows`）里跑的，当场 drop 会漏掉
    /// **正在播放的那个窗口**（其 atlas tile 残留，每手动切一次漏一片）。`Effect::Defer`
    /// 在本拍 `finish_update` flush，其时当前窗口已放回 `App.windows` → 所有入口（泵/自动切/
    /// 按键/按钮）都命中断言窗口；帧字节在 Arc 未 drop 前仍在、可重传，不丢渲染
    ///（同 markdown-wysiwyg `pending_math_drop` 的「延迟一帧释放」姿势）。
    pub(super) fn drop_old_frame(&mut self, old: Arc<RenderImage>, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            self.dropped_frame = Some(old.id.0);
        }
        cx.defer(move |app| app.drop_image(old, None));
    }


    /// 装新帧/切文件时换下**当前已上屏**的旧帧：旧 BGRA 帧的图集 tile 延迟到本拍更新末尾
    /// 释放（[`Self::drop_old_frame`]，见其注释）；旧 GPU 直通帧的共享槽回往 worker 池
    /// （跨线程只传句柄数值）。三个替换点共用：`install_bgra` / `install_gpu` / `open_folder_index`。
    pub(super) fn swap_display(&mut self, cx: &mut Context<Self>) {
        if let Some(old) = self.frame.take() {
            self.drop_old_frame(old, cx);
        }
        if let Some(g) = self.gpu_frame.take() {
            self.send_release(g.shared_handle);
        }
    }


    /// 把 BGRA 包成 `RenderImage` 上屏。`intrinsic` 优先保留打开期从 `params` 读到的
    /// **源尺寸**（decode-to-display 下采样后逐帧 w/h 是输出尺寸，宽高比一致即可）。
    pub(super) fn install_bgra(&mut self, w: u32, h: u32, bgra: Vec<u8>, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        // 帧字节移入 RenderImage（零拷贝）即持有时；截图在点击时经 `frame.as_bytes` 取回
        // P1-26：FFI 尺寸异常（`len != w*h*4`）时安全降级——不 `expect` 崩主线程；丢弃脏帧、
        // 保持最近一帧并打诊断行（缓冲曾过小 / 几何突增的帧数据已不可信）。
        let Some(buf) = RgbaImage::from_raw(w, h, bgra) else {
            eprintln!(
                "[video] 装帧尺寸异常，跳过本帧: {w}×{h} 需要 {}B",
                (w as u64) * (h as u64) * 4
            );
            return;
        };
        let new = Arc::new(RenderImage::new(vec![Frame::new(buf)]));
        // 旧帧被新帧替换的唯一点：显式释放被换下的旧图集 tile（P0 资源泄漏——视频每帧新建
        // `RenderImage` → 全局自增 `ImageId`，图集 key 必 miss → 新 tile 只进不出，长会话
        // VRAM/图集持续上涨；`ImageSource::Render` 源图只经 `drop_image` 下线，同 pdf/
        // markdown-wysiwyg 姿势。gpui 语义：删 sprite_atlas 键并卸载对应 backend tile；已在
        // 先前帧 present 的旧场景不含此键，安全。旧 GPU 帧共享槽一并回池（见 `swap_display`）。
        self.swap_display(cx);
        self.frame = Some(new);
        self.intrinsic.get_or_insert((w as f32, h as f32));
        if vprof_enabled() {
            VPROF_PRESENT.with(|acc| {
                let mut acc = acc.borrow_mut();
                acc.0 += 1;
                acc.1 += t0.elapsed();
                if acc.0 % 120 == 0 {
                    eprintln!(
                        "[VPROF] main present: {:.3}ms/帧（Frame/RenderImage 装配）",
                        acc.1.as_secs_f64() * 1000.0 / acc.0 as f64
                    );
                }
            });
        }
    }


    /// 清空 `overflow`（换代 / 切文件共用）：软越线帧随 drop 释放；GPU 越线帧的共享槽
    /// 回往 worker 池（不回流会在 decode-ahead 窗口内堆积耗尽槽池）。
    pub(super) fn clear_overflow(&mut self) {
        if let Some(o) = self.overflow.take() {
            self.drop_frame_item(o);
        }
    }


    /// 把共享槽释放消息发给 worker（跨线程只传数值；通道无界不阻塞；worker 已随换代/退出
    /// 死亡时 send 失败 = 槽随 worker 池一并释放，安全 no-op）。
    pub(super) fn send_release(&self, shared_handle: usize) {
        if let Some(tx) = &self.release_cmd {
            let _ = tx.send(shared_handle);
        }
    }


    /// 丢弃一个**未装屏**的帧：GPU 帧的共享槽回往 worker 池；软帧为纯字节无需回收。
    pub(super) fn drop_frame_item(&mut self, item: FrameItem) {
        if let FrameItem::Gpu(f) = item {
            self.send_release(f.shared_handle);
        }
    }


    /// 是否已有可显示帧（软 BGRA 或 GPU 直通任一）。
    pub(super) fn has_frame(&self) -> bool {
        self.frame.is_some() || self.gpu_frame.is_some()
    }


    /// 装一帧双设备 GPU 直通帧（worker 已 GPU 侧拷进共享槽）：旧软帧显式 drop（图集 tile）、
    /// 旧 GPU 帧槽回池，新句柄上屏。
    pub(super) fn install_gpu(&mut self, f: GpuFrame, cx: &mut Context<Self>) {
        let (w, h) = (f.w, f.h);
        self.swap_display(cx);
        self.gpu_frame = Some(f);
        self.intrinsic.get_or_insert((w as f32, h as f32));
    }


    /// 应用缩放因子并约束在 [`ZOOM_MIN`..=`ZOOM_MAX`]（钳位/无变化短路由
    /// [`pan_zoom::apply_zoom`]）；变化才 notify 重绘。
    pub(super) fn adjust_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if !pan_zoom::apply_zoom(&mut self.zoom, factor, ZOOM_MIN, ZOOM_MAX) {
            return;
        }
        cx.notify();
    }


    // ---- 放大后左键按住拖动画面（平移；未放大时 clamp 恒 0 = 无效果） ----

    /// 当前图在**画布区**（视口高 − 控制条高）内的 contain 显示尺寸（× 缩放）。
    /// 拖拽平移与 render 共用同一套尺寸，保证 clamp 与画面一致。
    pub(super) fn display_size(&self, vw: f32, vh: f32) -> (f32, f32) {
        let area_h = if self.locked {
            vh.max(1.0)
        } else {
            (vh - CONTROL_BAR_H).max(1.0)
        };
        let (iw, ih) = self.intrinsic.unwrap_or((0.0, 0.0));
        let (dw, dh) = Self::contain_fit(iw, ih, vw, area_h);
        (dw * self.zoom, dh * self.zoom)
    }


    /// 平移 clamp：放大后（`dw>vw` 或 `dh>vh`）允许在图内滑动，范围 = ±(图−视口)/2；
    /// 未放大时范围 0（永远居中，拖拽无效果）。以「视口中心 = 图中心」为 0 基准，
    /// 缩放/切图后范围变化由 render 逐帧重新 clamp 收敛（同 ImageViewer）。
    /// 逻辑收敛在 [`pan_zoom::clamp_pan`]。
    pub(super) fn clamp_pan(pan: (f32, f32), vw: f32, vh: f32, dw: f32, dh: f32) -> (f32, f32) {
        pan_zoom::clamp_pan(pan, vw, vh, dw, dh)
    }


    // --- M5 控制条 ---

    /// 可视区尺寸：`#video-main` 布局后的真实尺寸（`track_scroll` 记录）；首帧未布局
    /// （bounds 为 0）回退到窗口尺寸（对齐 ImageViewer `viewport_size` 的既有做法）。
    pub(super) fn viewport_size(&self, window: &Window) -> (f32, f32) {
        let s = self.viewport.bounds().size;
        let w = if s.width.as_f32() > 0.0 {
            s.width.as_f32()
        } else {
            window.bounds().size.width.as_f32()
        };
        let h = if s.height.as_f32() > 0.0 {
            s.height.as_f32()
        } else {
            window.bounds().size.height.as_f32()
        };
        (w, h)
    }


    /// 适应视口（Contain，**贴边无留白**）：取较小缩放比，一端正好贴住宽/高、另一端
    /// 居中留白（同 ImageViewer `contain_fit`——竖屏/横屏都保持原始比例缩放）。
    /// 逻辑收敛在 [`pan_zoom::contain_fit`]。
    pub(super) fn contain_fit(iw: f32, ih: f32, vw: f32, vh: f32) -> (f32, f32) {
        pan_zoom::contain_fit(iw, ih, vw, vh)
    }


    /// 播放进度比例（0..=1；未知时长时 0）。
    pub(super) fn ratio(&self) -> f32 {
        if self.duration > 0.0 {
            (self.playhead / self.duration) as f32
        } else {
            0.0
        }
    }


    /// `秒` → 时长字符串：m:ss，≥1h 自动 h:mm:ss（控制条时间与 F1 悬停时间浮层共用同一语义）。
    /// 逻辑收敛在 [`base::util::format_duration`]（与 music `duration_str` 共用一份实现，
    /// 音乐 ≥1h 一并显式统一为 h:mm:ss）。
    pub(super) fn fmt_time(secs: f64) -> String {
        base::util::format_duration(secs)
    }
}
