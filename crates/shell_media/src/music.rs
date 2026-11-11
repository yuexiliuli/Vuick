//! 音乐模式域逻辑。
//!
//! shell 对音乐播放器（`crates/music`）的**接线**：播放器引擎与 UI 核心逻辑不改动。
//! 本模块收敛路由/判定核心——什么路径进音乐模式（[`is_audio_path`]）、弹窗 toggle 状态
//! 判定（[`plan_music_toggle`]）、首次进目录判定（[`should_load_default_dir`]）、播放
//! 请求核心（[`play_path_in`]）。`MusicView` 惰性创建（连同 `MusicPlayer` 实体与
//! `ExitRequested` 订阅）与焦点/状态编排属宿主编排，留 shell 薄适配。

use std::path::{Path, PathBuf};

use gpui::App;
use music::MusicView;

/// 某路径是否应进入音乐模式（路由判定：音频文件 → 整窗音乐模式播放，不进文本/媒体
/// 选项卡路由；shell `open_file_here` 在 EPUB/PDF/图片/视频/文本分类**之前**按此拦截，
/// 覆盖文件树双击、对话框、拖放、会话恢复、最近打开等全部入口）。
pub fn is_audio_path(p: &Path) -> bool {
    music::is_audio_extension(p)
}

/// toggle 状态判定：当前弹窗角色 → 目标动作（关 → 开 / 开 → 关）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MusicTogglePlan {
    /// 关音乐模式（回编辑器界面，焦点还给活跃编辑区）。
    TurnOff,
    /// 开音乐模式。
    TurnOn,
}

/// 按当前 `music_mode` 判定 toggle 目标（关→开 / 开→关）。
pub fn plan_music_toggle(current_on: bool) -> MusicTogglePlan {
    if current_on {
        MusicTogglePlan::TurnOff
    } else {
        MusicTogglePlan::TurnOn
    }
}

/// 首次进入判定：MusicView 尚未加载任何目录 → 需载默认音乐目录（`UserPrefs::music_dir`
/// 或兜底）。
pub fn should_load_default_dir(view: &gpui::Entity<MusicView>, cx: &mut App) -> bool {
    view.read(cx).player.read(cx).dir().is_none()
}

/// 播放请求核心：`MusicPlayer::play_path` 载入并播放（路径不在当前库 → 以文件父目录作
/// 该次库）。shell 侧调前已 `ensure_music_view` 保证视图存在。不创建任何文本选项卡。
pub fn play_path_in(view: &gpui::Entity<MusicView>, path: PathBuf, cx: &mut App) {
    view.update(cx, |v, cx| {
        v.player.update(cx, |pl, cx| pl.play_path(&path, cx));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_audio_path_routing() {
        assert!(is_audio_path(Path::new("a.ogg")), "音频进音乐模式");
        assert!(is_audio_path(Path::new("b.MP3")), "大写扩展名大小写不敏感");
        assert!(!is_audio_path(Path::new("c.txt")), "非音频不进音乐模式");
    }

    #[test]
    fn plan_music_toggle_decision() {
        assert_eq!(plan_music_toggle(true), MusicTogglePlan::TurnOff, "开 → 关");
        assert_eq!(plan_music_toggle(false), MusicTogglePlan::TurnOn, "关 → 开");
    }
}
