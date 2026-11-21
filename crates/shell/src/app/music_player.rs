//! music_player：shell 对音乐播放器（`crates/music`）的集成层**宿主薄适配**。
//! 播放器引擎与 UI 核心逻辑不改动；**路由/判定核心**在 `shell_media::music`——什么
//! 路径进音乐模式（`is_audio_path`）、toggle 状态判定（`plan_music_toggle`）、首次进
//! 目录判定（`should_load_default_dir`）、播放请求（`play_path_in`）。本模块保留
//! `MusicView` 惰性创建（连同 `MusicPlayer` 实体与 `ExitRequested` 订阅）与焦点/状态
//! 编排（宿主编排）：
//!
//! - [`EditorApp::toggle_music_player`]：Ctrl+M / 菜单 / 命令触发，切换整窗音乐模式；
//! - [`EditorApp::open_music_path`]：打开音频文件 → 进入音乐模式并播放（不进文本/媒体
//!   选项卡路由，`file_ops::open_file` 源头拦截）；
//! - `MusicView` 惰性创建（连同其 `MusicPlayer` 实体）并订阅 `MusicEvent::ExitRequested`
//!   （MusicView 非全屏按 Esc 时发出 → 退出音乐模式）。
//!
//! 生命周期：`music` 视图一旦创建即跨模式常驻（退出音乐模式不销毁——后台播放继续）；
//! 随 EditorApp drop 自动释放实体与退订（gpui 实体作用域）。

use super::*;

impl EditorApp {
    /// 切换音乐模式：关 → 开（首次惰性创建 MusicView；尚未加载目录则载默认音乐目录）；
    /// 开 → 关（回编辑器界面，焦点还给活跃编辑区）。toggle 判定在
    /// [`shell_media::music::plan_music_toggle`]，首次进目录判定
    /// [`shell_media::music::should_load_default_dir`]。
    /// `#[doc(hidden)] pub`：`tests/music.rs`/`tests/media.rs` 音乐模式切换断言（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn toggle_music_player(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // toggle 判定先提纯成 owned plan（match 脚手架上 `self.model.borrow()` 的临时
        // 会被延命至整个 match——分派时 `borrow_mut()` 与之冲突，E0502）。
        let plan = shell_media::music::plan_music_toggle(self.model.borrow().music_mode);
        match plan {
            shell_media::music::MusicTogglePlan::TurnOff => {
                self.model.borrow_mut().music_mode = false;
                window.focus(&self.editor_focus_handle(cx), cx);
                cx.notify();
            }
            shell_media::music::MusicTogglePlan::TurnOn => {
                let view = self.ensure_music_view(cx);
                // 首次进入：尚未加载任何目录 → 载默认音乐目录（`UserPrefs::music_dir` 或兜底）。
                if shell_media::music::should_load_default_dir(&view, cx) {
                    view.update(cx, |v, cx| v.load_default_dir(cx));
                }
                self.model.borrow_mut().music_mode = true;
                window.focus(&view.read(cx).focus_handle(), cx);
                cx.notify();
            }
        }
    }

    /// 打开音频文件 → 进入音乐模式并播放：确保 MusicView 存在，`MusicPlayer::play_path`
    /// 载入并播放（路径不在当前库 → 以文件父目录作该次库）。播放请求核心在
    /// [`shell_media::music::play_path_in`]。不创建任何文本选项卡。
    pub(super) fn open_music_path(
        &mut self,
        p: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.ensure_music_view(cx);
        let path = p.clone();
        shell_media::music::play_path_in(&view, path, cx);
        self.model.borrow_mut().music_mode = true;
        window.focus(&view.read(cx).focus_handle(), cx);
        cx.notify();
    }

    /// 惰性创建 MusicView（连同其 `MusicPlayer`），并订阅 `ExitRequested` 事件
    /// （MusicView 非全屏按 Esc 时发出 → 关音乐模式并把焦点还给编辑区）。宿主编排。
    fn ensure_music_view(&mut self, cx: &mut Context<Self>) -> gpui::Entity<music::MusicView> {
        if let Some(v) = &self.music {
            return v.clone();
        }
        let player = cx.new(music::MusicPlayer::new);
        let view = cx.new(|cx| music::MusicView::new(player, cx));
        let sub = cx.subscribe(&view, |this, _view, ev: &music::MusicEvent, cx| {
            match *ev {
                music::MusicEvent::ExitRequested => {
                    this.model.borrow_mut().music_mode = false;
                    let focus = this.editor_focus_handle(cx);
                    let entity = cx.entity();
                    // 事件在窗口更新栈内派发（同步 update_window 会空转）→ 延后到主线程
                    // 任务里按实体找所属窗口恢复编辑区焦点。
                    cx.spawn(async move |_weak, cx| {
                        cx.update(|app| {
                            app.with_window(entity.entity_id(), |window, app| {
                                window.focus(&focus, app);
                            });
                        });
                    })
                    .detach();
                    cx.notify();
                }
            }
        });
        self.music = Some(view.clone());
        self._music_sub = Some(sub);
        view
    }
}
