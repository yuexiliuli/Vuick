//! 媒体窗口打开重定向**宿主薄适配**：查看菜单两个开关（`media_new_window` /
//! `reuse_media_window`）驱动 `open_file` 对音频/视频的分流——新开同进程独立窗口 / 复用
//! 已有媒体窗口 / 当前窗口。**纯决策**在 `shell_media::media_window`（`MediaOpenTarget` /
//! `MediaWindowKind` / `resolve_*` / `media_window_kind` / `is_media_path` /
//! `resolve_media_open_action` / `record_media_window_size` 模型段 / `open_target_kind`）；
//! 路由的**真正执行**（开新窗 / 跨窗口 `open_file_here`）与「最近其它媒体窗口」查询经
//! [`MediaHost`] 宿主服务面（`host_impl.rs`）分发，域不直读 `EditorAppRegistry`。
//!
//! 本模块保留宿主编排：`maybe_redirect_media_open` 包装（读模型输入 → 域决议 → 按
//! [`MediaOpenAction`] 分发执行 + 诊断探针）、开关翻转、尺寸落盘防抖调度。
//!
//! 分流只发生在 [`EditorApp::open_file`] 那一个入口（sidebar/files、palette、recent_panel、
//! file_dialog、init CLI 首参、会话恢复全部汇入它，故重定向对全部入口统一生效）。打开
//! **本体** [`EditorApp::open_file_here`]（file_ops.rs）**永不重定向**——新窗口 / 复用目标
//! 窗口直调它，避免「重定向 → 新窗再读同一 CLI 参 → 再开新窗」的循环。

use super::*;
use shell_media::media_window::MediaOpenAction;
use shell_media::MediaHost;

impl EditorApp {
    /// 本窗口是否「媒体窗口」（媒体打开复用目标判定）：音乐模式开启 或 活动选项卡为视频。
    /// 与 [`Self::media_window_kind`] 同源（`kind.is_some()`），保持单一事实源。
    /// `#[doc(hidden)] pub`：`tests/media.rs` 复用路由判定断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn is_media_window(&self) -> bool {
        self.media_window_kind().is_some()
    }

    /// 本窗口当前的「媒体窗口角色」：`music_mode` 开启 → [`MediaWindowKind::Music`]；
    /// 否则活动选项卡为视频 → [`MediaWindowKind::Video`]；两者都不是（文本/PDF/图片/空）
    /// → `None`。尺寸记录按角色分写 music / video 两组，None 时不记录（不覆盖）。
    /// 域决策 [`shell_media::media_window::media_window_kind`]。
    pub(super) fn media_window_kind(&self) -> Option<MediaWindowKind> {
        shell_media::media_window::media_window_kind(&self.model.borrow())
    }

    /// 记录媒体窗口尺寸到对应角色字段（音乐/视频分开），并在停顿时落盘到 config.json。
    /// 域逻辑 [`shell_media::media_window::record_media_window_size`]（模型段 + clamp +
    /// 去重），落盘防抖调度 [`Self::schedule_media_size_flush`] 与诊断/notify 宿主落地。
    /// 测试直接驱动（persist_prefs 在测试构建为 no-op，断言内存字段即可）。
    /// `#[doc(hidden)] pub`：`tests/media.rs` 尺寸记录/取值断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn record_media_window_size(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        let Some((kind, size)) =
            shell_media::media_window::record_media_window_size(&mut self.model.borrow_mut(), size)
        else {
            return;
        };
        // `&mut self.model.borrow_mut()` 的 guard 在语句结束已 drop，
        // 此处 `&self` 路径（schedule / 诊断）不再持有模型借用（防 RefCell 双借 panic）。
        self.schedule_media_size_flush(cx);
        if std::env::var_os("VUICK_DIAG").is_some() {
            eprintln!(
                "[MEDIAWNDSZ] record kind={kind:?} size={}x{}",
                size.width.as_f32().round() as i64,
                size.height.as_f32().round() as i64,
            );
        }
        cx.notify();
    }

    /// 把媒体窗口尺寸记录的落盘安排成防抖任务（可重入、代序作废，仿会话防抖姿势）：
    /// 停顿 [`shell_media::media_window::MEDIA_SIZE_FLUSH_DEBOUNCE_MS`] 后写一次
    /// config.json；期间再 resize → 旧单作废。调度骨架收敛到 [`base::util::SeqAfter::after_weak`]
    /// （弱句柄升级 + 静默丢弃）；内存字段已由 [`Self::record_media_window_size`] 更新
    /// （测试直接驱动，断言内存字段即可）。
    fn schedule_media_size_flush(&self, _cx: &mut Context<Self>) {
        // 防抖只挂在非测试构建（不残留定时任务）。弱句柄先单独绑定（同一表达式里 `_cx`
        // 可变实参与 `_cx.weak_entity()` 不可变借用冲突）。
        #[cfg(not(test))]
        {
            let weak = _cx.weak_entity();
            self.model.borrow().media_size_flush_debounce.after_weak(
                _cx,
                std::time::Duration::from_millis(
                    shell_media::media_window::MEDIA_SIZE_FLUSH_DEBOUNCE_MS,
                ),
                weak,
                move |this, _| {
                    this.persist_prefs();
                },
            );
        }
    }

    /// 记录主窗口窗口化尺寸到 [`AppModel::main_window_size`]，并在停顿时落盘到 config.json。
    /// 域逻辑 [`shell_core::actions::record_main_window_size`]（clamp + 去重），落盘防抖
    /// 调度 [`Self::schedule_main_size_flush`] 与诊断/notify 宿主落地。测试直接驱动
    /// （persist_prefs 在测试构建为 no-op，断言内存字段即可）。
    /// `#[doc(hidden)] pub`：`tests/main_window.rs` 尺寸记录断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn record_main_window_size(&mut self, size: Size<Pixels>, cx: &mut Context<Self>) {
        let Some(size) =
            shell_core::actions::record_main_window_size(&mut self.model.borrow_mut(), size)
        else {
            return;
        };
        // `&mut self.model.borrow_mut()` 的 guard 在语句结束已 drop，
        // 此处 `&self` 路径（schedule / 诊断）不再持有模型借用（防 RefCell 双借 panic）。
        self.schedule_main_size_flush(cx);
        if std::env::var_os("VUICK_DIAG").is_some() {
            eprintln!(
                "[MAINWNDSZ] record size={}x{}",
                size.width.as_f32().round() as i64,
                size.height.as_f32().round() as i64,
            );
        }
        cx.notify();
    }

    /// 把主窗口尺寸记录的落盘安排成防抖任务（可重入、代序作废，仿
    /// [`Self::schedule_media_size_flush`]）：停顿
    /// [`shell_core::actions::MAIN_SIZE_FLUSH_DEBOUNCE_MS`] 后写一次 config.json；
    /// 期间再 resize → 旧单作废。内存字段已由 [`Self::record_main_window_size`] 更新
    /// （测试直接驱动，断言内存字段即可）。
    fn schedule_main_size_flush(&self, _cx: &mut Context<Self>) {
        // 防抖只挂在非测试构建（不残留定时任务）。弱句柄先单独绑定（同一表达式里 `_cx`
        // 可变实参与 `_cx.weak_entity()` 不可变借用冲突）。
        #[cfg(not(test))]
        {
            let weak = _cx.weak_entity();
            self.model.borrow().main_size_flush_debounce.after_weak(
                _cx,
                std::time::Duration::from_millis(shell_core::actions::MAIN_SIZE_FLUSH_DEBOUNCE_MS),
                weak,
                move |this, _| {
                    this.persist_prefs();
                },
            );
        }
    }

    /// `open_file` 的重定向包装点：仅对「音频/视频 且 media_new_window 开启」做分流。
    /// 决策在 [`shell_media::media_window`]（`is_media_path` + `resolve_media_open_action`），
    /// 执行经 [`MediaHost`] 宿主服务面分发（开新窗 / 跨窗口路由）。返回 `true` = 已把
    /// 媒体交给新窗口 / 复用窗口处理（调用方直接 return）；`false` = 当前窗口打开
    /// （PDF/图片/文本/EPUB 与开关关闭时恒走此路）。
    pub(super) fn maybe_redirect_media_open(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let p = PathBuf::from(path.trim());
        // 打开墙钟诊断（VUICK_DIAG 守卫，平时零开销零行为）：记录「触发 open_file
        // 分流 → 决策 / 新窗口就绪」的墙钟，供轨 D 基线（headless 基准同口径对表）。
        let diag_t0 = if std::env::var_os("VUICK_DIAG").is_some() {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // 只对音频/视频做重定向；PDF/图片/文本/EPUB 与 `is_media_path` 判定结果完全一致
        // （同一扩展名清单），不受本开关影响。
        let is_media = shell_media::media_window::is_media_path(&p);
        if !is_media || !self.model.borrow().media_new_window {
            return false;
        }
        let self_is_media = self.is_media_window();
        let self_kind = self.media_window_kind();
        // 「最近其它媒体窗口」查询收敛到宿主服务（域不直读 EditorAppRegistry）；返回句柄 +
        // 角色（P1-C7 角色维度：决策据此拒绝「音乐窗口承接视频」）。
        let other = self.media_host_nearest_other_media_window(cx);
        let (other_handle, other_kind) = match other {
            Some((wh, k)) => (Some(wh), Some(k)),
            None => (None, None),
        };
        // 目标媒体种类：按扩展名（音频 → Music / 视频 → Video，域决策 `open_target_kind`；
        // 角色维度的「目标」半边）。
        let target_kind = shell_media::media_window::open_target_kind(&p);
        let action = shell_media::media_window::resolve_media_open_action(
            self.model.borrow().media_new_window,
            self.model.borrow().reuse_media_window,
            self_is_media,
            other_handle,
            // 发起窗口无任何文件选项卡（空窗/欢迎页）→ 媒体就地打开（域决策
            // `self_has_tabs` 语义见 shell_media::media_window::resolve_media_open_target）。
            !self.model.borrow().tabs.is_empty(),
            self_kind,
            other_kind,
            target_kind,
        );
        // K39 姿势诊断探针：真机冒烟取证用（VUICK_DIAG 守卫，平时零开销）。
        let windows_n = cx
            .try_global::<EditorAppRegistry>()
            .map(|r| r.windows.len())
            .unwrap_or(0);
        if let Some(t0) = diag_t0 {
            eprintln!(
                "[MEDIAWND] path={path} target={action:?} windows={windows_n} wall={:.1}ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        match action {
            MediaOpenAction::Here => false,
            MediaOpenAction::NewWindow => {
                self.media_host_open_media_new_window(&p, window, cx);
                true
            }
            MediaOpenAction::OpenIn(wh) => {
                // 决策与「扫描到目标窗口已被关闭」的竞态兜底（新窗口）已收在域
                // `resolve_media_open_action` 内——此处 `OpenIn` 恒带有效句柄。
                self.media_host_route_media_to_window(&p, wh, cx);
                true
            }
        }
    }

    /// 「媒体在新窗口打开」开关：翻转 + 立即持久化（与其它偏好同款落盘）。
    /// `#[doc(hidden)] pub`：`tests/media.rs` 开关翻转断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_media_new_window(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 同一语句两次 `borrow_mut()`（LHS guard 存活时求值 RHS）运行期双借
        // panic → 一次取 guard 内完成「读旧值后翻转」。
        let mut m = self.model.borrow_mut();
        m.media_new_window = !m.media_new_window;
        drop(m);
        self.persist_prefs();
        cx.notify();
    }

    /// 「复用已有媒体窗口」开关：仅 `media_new_window` 开启时翻转（关闭时该开关无意义，
    /// 菜单项置灰；此处兜底拒绝，双保险防半开态）。
    /// `#[doc(hidden)] pub`：`tests/media.rs` 开关翻转断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_reuse_media_window(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model.borrow().media_new_window {
            return;
        }
        // 同语句双 `borrow_mut()` panic → 一次 guard 内翻转。
        let mut m = self.model.borrow_mut();
        m.reuse_media_window = !m.reuse_media_window;
        drop(m);
        self.persist_prefs();
        cx.notify();
    }
}
