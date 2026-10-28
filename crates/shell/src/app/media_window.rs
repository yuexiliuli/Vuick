//! 媒体窗口打开重定向：查看菜单两个开关（`media_new_window` / `reuse_media_window`）驱动
//! `open_file` 对音频/视频的分流——新开同进程独立窗口 / 复用已有媒体窗口 / 当前窗口。
//!
//! 分流只发生在 [`EditorApp::open_file`] 那一个入口（sidebar/files、palette、recent_panel、
//! file_dialog、init CLI 首参、会话恢复全部汇入它，故重定向对全部入口统一生效）。打开
//! **本体** [`EditorApp::open_file_here`]（file_ops.rs）**永不重定向**——新窗口 / 复用目标
//! 窗口直调它，避免「重定向 → 新窗再读同一 CLI 参 → 再开新窗」的循环。
//!
//! 「媒体窗口」定义 = 窗口 `music_mode` 开启 **或** 活动选项卡为视频（`MediaReader::Video`）；
//! 复用目标 = [`EditorAppRegistry`](crate::app::EditorAppRegistry) 中**最近的媒体窗口**
//! （注册序最新 = 列表逆序首个）。
//! 已知行为：重定向对 CLI 首参 / 会话恢复同样生效（两者经 `open_file` 汇入）；媒体窗口由
//! `new_media_window` 构造（不读 CLI 首参、不恢复会话），避免 CLI 媒体参数在重定向链上
//! 被反复重读成循环、以及重复恢复全量会话（见 init.rs `new_impl` 的 `startup` 门控）。

use super::*;

/// 媒体打开去向（[`EditorApp::open_file`] 对音频/视频的重定向纯决策结果）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MediaOpenTarget {
    /// 当前窗口打开（现行行为）。
    Here,
    /// 复用 registry 中最近的已有媒体窗口打开。
    ReuseExisting,
    /// 新开同进程独立窗口打开。
    NewWindow,
}

/// 媒体窗口的「当前角色」（尺寸记录按角色分开写 music / video 两组）：
/// 与「媒体窗口」判定同源（music_mode → Music；否则活动选项卡为视频 → Video）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum MediaWindowKind {
    /// 音乐模式（整窗 MusicView）。
    Music,
    /// 视频查看器（活动选项卡 `MediaReader::Video`）。
    Video,
}

/// 媒体窗口尺寸落盘防抖（毫秒）：连续 resize 停顿该时长后 persist 一次，不逐事件写盘。
/// 对齐会话防抖 SESSION_FLUSH_DEBOUNCE_MS 的 300ms 口径。防抖体被 `#[cfg(not(test))]`
/// 门控 → 测试构建不引用本常量（避免 dead_code 噪声），语义不变。
#[cfg(not(test))]
const MEDIA_SIZE_FLUSH_DEBOUNCE_MS: u64 = 300;

/// 新开媒体窗口的最小尺寸护栏（逻辑像素）：防用户把窗口拖到过小/记录值退化。
const MIN_MEDIA_WIN_W: f32 = 200.0;
const MIN_MEDIA_WIN_H: f32 = 160.0;

/// 纯决策函数：按「媒体打开行为表」逐行解析打开去向。
///
///   | media_new_window | reuse_media_window | 当前窗口是媒体 | 另有媒体窗口 | 结果        |
///   | 关(any)          | any                | any            | any            | Here        |
///   | 开               | 开                 | 是             | any            | Here        |
///   | 开               | 开                 | 否             | 是             | ReuseExisting|
///   | 开               | 开                 | 否             | 否             | NewWindow   |
///   | 开               | 关                 | any            | any            | NewWindow   |
///
/// 纯函数、无副作用，单测直接覆盖全真值组合（见 tests/media_window.rs）。
pub(super) fn resolve_media_open_target(
    media_new_window: bool,
    reuse: bool,
    self_is_media: bool,
    has_other_media: bool,
) -> MediaOpenTarget {
    if !media_new_window {
        MediaOpenTarget::Here
    } else if !reuse {
        MediaOpenTarget::NewWindow
    } else if self_is_media {
        MediaOpenTarget::Here
    } else if has_other_media {
        MediaOpenTarget::ReuseExisting
    } else {
        MediaOpenTarget::NewWindow
    }
}

/// 决定「新开媒体窗口按哪份记录尺寸」：发起者是**媒体窗口** → 自身内存即最新（本窗刚
/// `record_media_window_size` 过 / 当前角色同组，防抖未落盘期间也正确）；否则（主窗口
/// 等非媒体窗）**内存可能陈旧**——媒体窗口 resize 自动写盘后不广播回发起窗内存，若只看
/// 内存会退化成「调好音乐窗尺寸→关→再开仍是旧尺寸」，故以 config 盘上最新记录优先、
/// 内存兜底（内存 = 构造期从 prefs 读入的现值）。
pub(super) fn resolve_recorded_size(
    kind: MediaWindowKind,
    self_is_media: bool,
    mem_music: Option<Size<Pixels>>,
    mem_video: Option<Size<Pixels>>,
    disk_music: Option<(f32, f32)>,
    disk_video: Option<(f32, f32)>,
) -> Option<Size<Pixels>> {
    let pick =
        |mem: Option<Size<Pixels>>, disk: Option<(f32, f32)>| {
            if self_is_media {
                mem
            } else {
                disk.map(|(w, h)| gpui::size(px(w), px(h))).or(mem)
            }
        };
    match kind {
        MediaWindowKind::Music => pick(mem_music, disk_music),
        MediaWindowKind::Video => pick(mem_video, disk_video),
    }
}

impl EditorApp {
    /// 本窗口是否「媒体窗口」（媒体打开复用目标判定）：音乐模式开启 或 活动选项卡为视频。
    /// 与 [`Self::media_window_kind`] 同源（`kind.is_some()`），保持单一事实源（行为表回归
    /// 由 tests/media_window.rs 既有 10 测试覆盖）。
    pub(super) fn is_media_window(&self) -> bool {
        self.media_window_kind().is_some()
    }

    /// 本窗口当前的「媒体窗口角色」：`music_mode` 开启 → [`MediaWindowKind::Music`]；
    /// 否则活动选项卡为视频 → [`MediaWindowKind::Video`]；两者都不是（文本/PDF/图片/空）
    /// → `None`。尺寸记录按角色分写 music / video 两组，None 时不记录（不覆盖）。
    pub(super) fn media_window_kind(&self) -> Option<MediaWindowKind> {
        if self.music_mode {
            return Some(MediaWindowKind::Music);
        }
        #[cfg(feature = "video")]
        if self
            .tabs
            .get(self.active_tab)
            .map(|t| matches!(t.reader, MediaReader::Video(_)))
            .unwrap_or(false)
        {
            return Some(MediaWindowKind::Video);
        }
        None
    }

    /// 记录媒体窗口尺寸到对应角色字段（音乐/视频分开），并在停顿时落盘到 config.json
    /// （连续 resize 不逐事件写盘，K10/K11）。去重：尺寸与上次记录一致不写；最小尺寸护栏
    /// 在写入前 clamp。角色为 `None`（非媒体窗口）时不记录（不覆盖）。
    /// 测试直接驱动（persist_prefs 在测试构建为 no-op，断言内存字段即可）。
    pub(super) fn record_media_window_size(
        &mut self,
        size: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = self.media_window_kind() else {
            return;
        };
        // 最小尺寸护栏（防退化到过小 / 记录值被极端 resize 污染）。
        let size = gpui::size(
            px(size.width.as_f32().max(MIN_MEDIA_WIN_W)),
            px(size.height.as_f32().max(MIN_MEDIA_WIN_H)),
        );
        let slot = match kind {
            MediaWindowKind::Music => &mut self.music_window_size,
            MediaWindowKind::Video => &mut self.video_window_size,
        };
        if *slot == Some(size) {
            return; // 尺寸未变：不重复标记 / 不重复写盘
        }
        *slot = Some(size);
        self.schedule_media_size_flush(cx);
        if std::env::var_os("GPUI_EDITOR_DIAG").is_some() {
            eprintln!(
                "[MEDIAWNDSZ] record kind={kind:?} size={}x{}",
                size.width.as_f32().round() as i64,
                size.height.as_f32().round() as i64,
            );
        }
        cx.notify();
    }

    /// 把媒体窗口尺寸记录的落盘安排成防抖任务（可重入、代序作废，仿会话防抖姿势）：
    /// 停顿 [`MEDIA_SIZE_FLUSH_DEBOUNCE_MS`] 后写一次 config.json；期间再 resize → 旧单作废。
    /// 测试构建不注册定时任务（防残留），内存字段已由 `record_media_window_size` 更新。
    fn schedule_media_size_flush(&self, _cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let weak = _cx.weak_entity();
            self.media_size_flush_debounce.after(
                _cx,
                std::time::Duration::from_millis(MEDIA_SIZE_FLUSH_DEBOUNCE_MS),
                move |app| {
                    let _ = weak.update(app, |this, _| {
                        this.persist_prefs();
                    });
                },
            );
        }
    }

    /// `open_file` 的重定向包装点：仅对「音频/视频 且 media_new_window 开启」做分流。
    /// 返回 `true` = 已把媒体交给新窗口 / 复用窗口处理（调用方直接 return）；
    /// `false` = 当前窗口打开（PDF/图片/文本/EPUB 与开关关闭时恒走此路）。
    pub(super) fn maybe_redirect_media_open(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let p = PathBuf::from(path.trim());
        // 打开墙钟诊断（GPUI_EDITOR_DIAG 守卫，平时零开销零行为）：记录「触发 open_file
        // 分流 → 决策 / 新窗口就绪」的墙钟，供轨 D 基线（headless 基准同口径对表）。
        let diag_t0 = if std::env::var_os("GPUI_EDITOR_DIAG").is_some() {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // 只对音频/视频做重定向；PDF/图片/文本/EPUB 与 `is_video_path` 判定结果
        // 完全一致（同一扩展名清单），不受本开关影响。
        #[cfg(feature = "video")]
        let is_media = music::is_audio_extension(&p) || Self::is_video_path(&p);
        #[cfg(not(feature = "video"))]
        let is_media = music::is_audio_extension(&p);
        if !is_media || !self.media_new_window {
            return false;
        }
        let self_is_media = self.is_media_window();
        let other = self.nearest_other_media_window(cx);
        let target = resolve_media_open_target(
            self.media_new_window,
            self.reuse_media_window,
            self_is_media,
            other.is_some(),
        );
        // K39 姿势诊断探针：真机冒烟取证用（GPUI_EDITOR_DIAG 守卫，平时零开销）。
        let windows_n = cx
            .try_global::<EditorAppRegistry>()
            .map(|r| r.windows.len())
            .unwrap_or(0);
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWND] path={path} target={target:?} windows={windows_n} wall={:.1}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        match target {
            MediaOpenTarget::Here => false,
            MediaOpenTarget::NewWindow => {
                self.open_media_in_new_window(&p, window, cx);
                true
            }
            MediaOpenTarget::ReuseExisting => {
                if let Some((_entity, wh)) = other {
                    // 决策与扫描之间目标窗口被关闭的极端竞态 → 兜底新开窗口（行为表 NewWindow 行）。
                    self.route_media_to_window(&p, wh, cx);
                } else {
                    self.open_media_in_new_window(&p, window, cx);
                }
                true
            }
        }
    }

    /// 在全局注册表里找「最近的媒体窗口」：列表逆序首个媒体窗口 = 最近创建的媒体窗口；
    /// 排除自身（按实体 id 比对）。返回强实体 + 窗口句柄（消费方用它直调目标窗口本体）。
    fn nearest_other_media_window(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(gpui::Entity<EditorApp>, AnyWindowHandle)> {
        if !cx.has_global::<EditorAppRegistry>() {
            return None;
        }
        let self_id = cx.entity().entity_id();
        let registry = cx.global::<EditorAppRegistry>();
        registry.windows.iter().rev().find_map(|(weak, wh)| {
            let entity = weak.upgrade()?;
            if entity.entity_id() == self_id {
                return None;
            }
            let is_media = entity.read_with(cx, |app, _| app.is_media_window());
            is_media.then_some((entity, *wh))
        })
    }

    /// 新开同进程独立窗口并打开该媒体：`open_window` 闭包内只造 app + root（构造期不
    /// 打开文件——`new_media_window` 不读 CLI 首参/不恢复会话，防重定向循环与重复恢复），
    /// 窗口创建成功后经窗口句柄补开媒体，规避构造期内调 `open_file_here` 的时序风险。
    /// 发起窗口内容零改动。
    /// 窗口尺寸：本次媒体种类（音频→音乐组 / 视频→视频组）有记录尺寸 → 按该尺寸居中开；
    /// 未记录 → **跟随当前窗口大小**（`window.bounds()`，解决「新窗口比原窗口大不一致」）。
    fn open_media_in_new_window(
        &mut self,
        p: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owned = p.to_string_lossy().into_owned();
        // 首帧墙钟诊断（GPUI_EDITOR_DIAG 守卫，平时零开销零行为）：触发起点 = 本函数
        // 入口；下方 [MEDIAWNDSZ] open / 路由完成两处补 wall 字段（轨 D 基线）。
        let diag_t0 = if std::env::var_os("GPUI_EDITOR_DIAG").is_some() {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // 按本次媒体种类取对应记录尺寸（新窗口尚未是媒体窗口，按扩展名决定角色即可）。
        let kind = if music::is_audio_extension(p) {
            MediaWindowKind::Music
        } else {
            MediaWindowKind::Video
        };
        // 取该角色记录尺寸：发起者是**媒体窗口**时自身内存即最新（本窗刚 record 过 / 当前
        // 角色同组）；否则（主窗口等非媒体窗）内存可能陈旧——媒体窗口 resize 自动写盘后
        // 不会广播到主窗口内存，会退化成「改好音乐窗尺寸→关→再开仍是旧尺寸」。此时以
        // config 盘上最新记录优先、内存兜底（`UserPrefs::load()` 每次读盘，代价仅打开动作）。
        let disk = UserPrefs::load();
        let recorded = resolve_recorded_size(
            kind,
            self.media_window_kind().is_some(),
            self.music_window_size,
            self.video_window_size,
            disk.music_window_size,
            disk.video_window_size,
        );
        let bounds = match recorded {
            Some(size) => {
                // 最小尺寸护栏（防记录值退化到过小）：clamp 后居中于主显示器。
                let size = gpui::size(
                    px(size.width.as_f32().max(MIN_MEDIA_WIN_W)),
                    px(size.height.as_f32().max(MIN_MEDIA_WIN_H)),
                );
                Bounds::centered(None, size, cx)
            }
            None => window.bounds(),
        };
        let titlebar_options = gpui::TitlebarOptions {
            title: Some("GPUI 编辑器".into()),
            appears_transparent: true,
            ..Default::default()
        };
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWNDSZ] open kind={kind:?} size={}x{} follow-source={} wall={:.1}ms",
                bounds.size.width.as_f32().round() as i64,
                bounds.size.height.as_f32().round() as i64,
                recorded.is_none(),
                t0.elapsed().as_secs_f64() * 1000.0,
            );
        }
        let handle: AnyWindowHandle = match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar_options),
                ..Default::default()
            },
            |new_window, cx| {
                let view = cx.new(|cx| EditorApp::new_media_window(new_window, cx));
                cx.new(|cx| ui::root::Root::new(view, new_window, cx))
            },
        ) {
            Ok(h) => h.into(),
            Err(e) => {
                // 新窗创建失败（罕见 / 平台禁用）→ 回退当前窗口打开，保证媒体仍可用。
                self.status = SharedString::from(format!("打开媒体失败：{e}"));
                self.status_ok = false;
                self.open_file_here(&owned, window, cx);
                return;
            }
        };
        self.route_media_to_window(p, handle, cx);
        // 墙钟：触发 → 新窗口已创建 + 媒体已路由到目标窗口（`route_media_to_window` 内
        // 直调目标窗口 `open_file_here` 同步打开）≈ 新窗口首帧前。
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWND] routed path={owned} wall={:.1}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    /// 把媒体文件经目标窗口句柄转交该窗口 EditorApp 本体打开（同进程跨窗口实体 update；
    /// 复用既有 `open_file_here`，目标窗口注册表已含自身）。目标窗口可能聚焦（打开动作
    /// 里的 window.focus），复用已有媒体窗口时属预期行为。
    fn route_media_to_window(
        &self,
        p: &std::path::Path,
        wh: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let owned = p.to_string_lossy().into_owned();
        let _ = wh.update(cx, |root, window, cx| {
            // AnyWindowHandle::update 给的是窗口根视图（ui::root::Root），经
            // Root::view() 下转型取承载的 EditorApp（root.rs 注释明确此用途）。
            if let Ok(root) = root.downcast::<ui::root::Root>() {
                let view = root.read(cx).view().clone();
                if let Ok(app) = view.downcast::<EditorApp>() {
                    let _ = app.update(cx, |app, cx| app.open_file_here(&owned, window, cx));
                }
            }
        });
    }

    /// 「媒体在新窗口打开」开关：翻转 + 立即持久化（与其它偏好同款落盘）。
    pub(super) fn toggle_media_new_window(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.media_new_window = !self.media_new_window;
        self.persist_prefs();
        cx.notify();
    }

    /// 「复用已有媒体窗口」开关：仅 `media_new_window` 开启时翻转（关闭时该开关无意义，
    /// 菜单项置灰；此处兜底拒绝，双保险防半开态）。
    pub(super) fn toggle_reuse_media_window(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.media_new_window {
            return;
        }
        self.reuse_media_window = !self.reuse_media_window;
        self.persist_prefs();
        cx.notify();
    }
}
