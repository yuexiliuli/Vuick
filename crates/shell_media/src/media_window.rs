//! 媒体窗口打开**纯决策**（2026-11-11 迁自 shell 的 app/media_window.rs）。
//!
//! 查看菜单两个开关（`media_new_window` / `reuse_media_window`）驱动 `open_file` 对
//! 音频/视频的分流——新开同进程独立窗口 / 复用已有媒体窗口 / 当前窗口。分流只发生在
//! `open_file` 那一个入口（sidebar/files、palette、recent_panel、file_dialog、init CLI
//! 首参、会话恢复全部汇入它，故重定向对全部入口统一生效）；打开**本体** `open_file_here`
//! （shell 侧）永不重定向，避免「重定向 → 新窗再读同一 CLI 参 → 再开新窗」的循环。
//!
//! 本模块收敛**决策与决策所需输入**：`MediaOpenTarget` / `MediaWindowKind` /
//! `resolve_media_open_target` / `resolve_recorded_size` / `media_window_kind` /
//! `is_media_path` / `resolve_media_open_action` / `record_media_window_size` 模型段 /
//! `open_target_kind`。路由的「真正执行」（开新窗、`open_file_here`）经 [`crate::MediaHost`]
//! 宿主服务分发；`EditorAppRegistry` 依赖 → `MediaHost::media_host_nearest_other_media_window`
//! 宿主返回，本模块不直读 registry。
//!
//! 已知行为：重定向对 CLI 首参 / 会话恢复同样生效（两者经 `open_file` 汇入）；媒体窗口由
//! `new_media_window` 构造（不读 CLI 首参、不恢复会话），避免 CLI 媒体参数在重定向链上
//! 被反复重读成循环、以及重复恢复全量会话。

use std::path::Path;

use gpui::{px, AnyWindowHandle, Pixels, Size};
use shell_core::AppModel;
#[cfg(feature = "video")]
use shell_core::MediaReader;

/// 媒体打开去向（`open_file` 对音频/视频的重定向纯决策结果）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaOpenTarget {
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
pub enum MediaWindowKind {
    /// 音乐模式（整窗 MusicView）。
    Music,
    /// 视频查看器（活动选项卡 `MediaReader::Video`）。
    Video,
}

/// 媒体窗口尺寸落盘防抖（毫秒）：连续 resize 停顿该时长后 persist 一次，不逐事件写盘。
/// 对齐会话防抖 SESSION_FLUSH_DEBOUNCE_MS 的 300ms 口径（宿主 `schedule_media_size_flush`
/// 引用；`pub` 避免测试构建 dead_code 噪声）。
pub const MEDIA_SIZE_FLUSH_DEBOUNCE_MS: u64 = 300;

/// 新开媒体窗口的最小尺寸护栏（逻辑像素）：防用户把窗口拖到过小/记录值退化。
pub const MIN_MEDIA_WIN_W: f32 = 200.0;
pub const MIN_MEDIA_WIN_H: f32 = 160.0;

/// 该路径是否触发媒体打开重定向（音频/视频；PDF/图片/文本/EPUB 恒 false）。视频半边与
/// shell 侧 `is_video_path`（`classify == PathKind::Video`）同源——`video::is_video_extension`
/// 是「是否存在该媒体」的唯一来源，两者一致，不受本开关影响。
#[cfg(feature = "video")]
pub fn is_media_path(p: &Path) -> bool {
    music::is_audio_extension(p) || video::is_video_extension(p)
}

/// 该路径是否触发媒体打开重定向（--no-default-features：视频整链关闭，仅音频）。
#[cfg(not(feature = "video"))]
pub fn is_media_path(p: &Path) -> bool {
    music::is_audio_extension(p)
}

/// 纯决策函数：按「媒体打开行为表」逐行解析打开去向。
///
///   | media_new_window | reuse_media_window | 当前窗口是媒体 | 另有媒体窗口 | 结果        |
///   | 关(any)          | any                | any            | any            | Here        |
///   | 开               | 开                 | 是             | any            | Here        |
///   | 开               | 开                 | 否             | 是             | ReuseExisting|
///   | 开               | 开                 | 否             | 否             | NewWindow   |
///   | 开               | 关                 | any            | any            | NewWindow   |
///
/// 纯函数、无副作用，单测直接覆盖全真值组合。
pub fn resolve_media_open_target(
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

/// 路由动作决议（对 [`MediaOpenTarget`] 落地一层）：把「复用已有媒体窗口」与「决策与
/// 扫描之间目标窗口被关闭的极端竞态兜底」（行为表 NewWindow 行）收进同一决议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaOpenAction {
    /// 当前窗口打开。
    Here,
    /// 新开同进程独立窗口。
    NewWindow,
    /// 复用已有媒体窗口（目标句柄已知）。
    OpenIn(AnyWindowHandle),
}

/// 由开关 / 窗口角色 / 候选媒体窗口句柄决议 [`MediaOpenAction`]（`open_file` 分流一次性
/// 决策：`resolve_media_open_target` + 竞态兜底的合并层）。
pub fn resolve_media_open_action(
    media_new_window: bool,
    reuse: bool,
    self_is_media: bool,
    other: Option<AnyWindowHandle>,
) -> MediaOpenAction {
    match resolve_media_open_target(media_new_window, reuse, self_is_media, other.is_some()) {
        MediaOpenTarget::Here => MediaOpenAction::Here,
        MediaOpenTarget::NewWindow => MediaOpenAction::NewWindow,
        MediaOpenTarget::ReuseExisting => match other {
            Some(wh) => MediaOpenAction::OpenIn(wh),
            None => MediaOpenAction::NewWindow,
        },
    }
}

/// 决定「新开媒体窗口按哪份记录尺寸」：发起者是**媒体窗口** → 自身内存即最新（本窗刚
/// `record_media_window_size` 过 / 当前角色同组，防抖未落盘期间也正确）；否则（主窗口
/// 等非媒体窗）**内存可能陈旧**——媒体窗口 resize 自动写盘后不广播回发起窗内存，若只看
/// 内存会退化成「调好音乐窗尺寸→关→再开仍是旧尺寸」，故以 config 盘上最新记录优先、
/// 内存兜底（内存 = 构造期从 prefs 读入的现值）。
pub fn resolve_recorded_size(
    kind: MediaWindowKind,
    self_is_media: bool,
    mem_music: Option<Size<Pixels>>,
    mem_video: Option<Size<Pixels>>,
    disk_music: Option<(f32, f32)>,
    disk_video: Option<(f32, f32)>,
) -> Option<Size<Pixels>> {
    let pick = |mem: Option<Size<Pixels>>, disk: Option<(f32, f32)>| {
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

/// 本窗口当前的「媒体窗口角色」：`music_mode` 开启 → [`MediaWindowKind::Music`]；
/// 否则活动选项卡为视频 → [`MediaWindowKind::Video`]；两者都不是（文本/PDF/图片/空）
/// → `None`。尺寸记录按角色分写 music / video 两组，None 时不记录（不覆盖）。
/// 与「媒体窗口」判定同源（`kind.is_some()`），保持单一事实源（行为表回归由宿主/集成
/// 测试覆盖）。
pub fn media_window_kind(m: &AppModel) -> Option<MediaWindowKind> {
    if m.music_mode {
        return Some(MediaWindowKind::Music);
    }
    #[cfg(feature = "video")]
    if m.tabs
        .get(m.active_tab)
        .map(|t| matches!(t.reader, MediaReader::Video(_)))
        .unwrap_or(false)
    {
        return Some(MediaWindowKind::Video);
    }
    None
}

/// 记录媒体窗口尺寸到对应角色字段（音乐/视频分开，`record_media_window_size` 模型段）。
/// 最小尺寸护栏在写入前 clamp；去重：尺寸与上次记录一致不写。返回 `Some((角色, 最终
/// 尺寸))` = 有实际写入（宿主据此排防抖落盘 + 诊断 + notify）；`None` = 无操作（非媒体
/// 窗口 / 尺寸未变）。
pub fn record_media_window_size(
    m: &mut AppModel,
    size: Size<Pixels>,
) -> Option<(MediaWindowKind, Size<Pixels>)> {
    let kind = media_window_kind(m)?;
    // 最小尺寸护栏（防退化到过小 / 记录值被极端 resize 污染）。
    let size = gpui::size(
        px(size.width.as_f32().max(MIN_MEDIA_WIN_W)),
        px(size.height.as_f32().max(MIN_MEDIA_WIN_H)),
    );
    let slot = match kind {
        MediaWindowKind::Music => &mut m.music_window_size,
        MediaWindowKind::Video => &mut m.video_window_size,
    };
    if *slot == Some(size) {
        return None;
    }
    *slot = Some(size);
    Some((kind, size))
}

/// 新开媒体窗口按本次媒体种类决定「用哪组记录尺寸」（音频 → 音乐组；否则视频组；新窗口
/// 尚未是媒体窗口，按扩展名决定角色即可）。
pub fn open_target_kind(p: &Path) -> MediaWindowKind {
    if music::is_audio_extension(p) {
        MediaWindowKind::Music
    } else {
        MediaWindowKind::Video
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_core::AppModel;

    /// `resolve_media_open_target` 与行为表逐行一致（16 组真值全覆盖）。
    #[test]
    fn resolve_media_open_target_behavior_table() {
        use MediaOpenTarget::*;
        for media_new in [false, true] {
            for reuse in [false, true] {
                for self_media in [false, true] {
                    for other in [false, true] {
                        let got = resolve_media_open_target(media_new, reuse, self_media, other);
                        // 逐行按行为表推导期望（与实现同表，防改乱回归断言）。
                        let want = if !media_new {
                            Here
                        } else if !reuse {
                            NewWindow
                        } else if self_media {
                            Here
                        } else if other {
                            ReuseExisting
                        } else {
                            NewWindow
                        };
                        assert_eq!(
                            got, want,
                            "行为表: media_new={media_new} reuse={reuse} self_media={self_media} other={other}"
                        );
                    }
                }
            }
        }
    }

    /// `resolve_media_open_action` 把 `resolve_media_open_target` 落地成动作（无句柄的
    /// 纯布尔决策面全覆盖；`OpenIn` 臂只差句柄装订，由集成测试经真实窗口句柄覆盖）。
    #[test]
    fn resolve_media_open_action_decision() {
        // 配置关 → Here
        assert_eq!(
            resolve_media_open_action(false, true, false, None),
            MediaOpenAction::Here
        );
        // 开 + 复用关 → NewWindow
        assert_eq!(
            resolve_media_open_action(true, false, false, None),
            MediaOpenAction::NewWindow
        );
        // 开 + 复用开 + 自身是媒体 → Here
        assert_eq!(
            resolve_media_open_action(true, true, true, None),
            MediaOpenAction::Here
        );
        // 开 + 复用开 + 无其它媒体窗口 → NewWindow
        assert_eq!(
            resolve_media_open_action(true, true, false, None),
            MediaOpenAction::NewWindow
        );
        // 开 + 复用开 + 由宿主给句柄：本函数只透传（不作为候选判定——None 与 Some 语义
        // 分界由宿主扫描负责；此处保证无句柄不进入 OpenIn）。
    }

    /// `media_window_kind` 按模型判定：空/文本窗口 None；music_mode → Music。
    #[test]
    fn media_window_kind_from_model() {
        let m = AppModel::default();
        assert_eq!(media_window_kind(&m), None, "空窗口不是媒体窗口");
        let mut m = m;
        m.music_mode = true;
        assert_eq!(media_window_kind(&m), Some(MediaWindowKind::Music));
    }

    /// `resolve_recorded_size` 取值语义：媒体发起者只看自身内存；非媒体发起者盘上最新
    /// 优先、内存兜底。
    #[test]
    fn resolve_recorded_size_music_video_groups() {
        let ms = gpui::size(px(800.), px(600.));
        let vs = gpui::size(px(900.), px(540.));
        // 媒体发起者：只看自身内存（盘上内容忽略——防抖窗口内盘上尚旧）。
        assert_eq!(
            resolve_recorded_size(
                MediaWindowKind::Music,
                true,
                Some(ms),
                Some(vs),
                Some((100.0, 100.0)),
                Some((200.0, 200.0)),
            ),
            Some(ms),
            "媒体发起者用自身内存"
        );
        // 非媒体发起者：盘上视频最新记录优先。
        assert_eq!(
            resolve_recorded_size(
                MediaWindowKind::Video,
                false,
                Some(ms),
                Some(vs),
                Some((100.0, 100.0)),
                Some((960.0, 540.0)),
            ),
            Some(gpui::size(px(960.), px(540.))),
            "主窗口发起：盘上视频最新记录优先"
        );
        // 盘上无记录 → 内存兜底（构造期从 prefs 读入的现值）。
        assert_eq!(
            resolve_recorded_size(MediaWindowKind::Music, false, Some(ms), Some(vs), None, None),
            Some(ms),
            "盘上无 → 内存兜底"
        );
    }

    /// `is_media_path`：音频/视频（feature 门控）→ 重定向；文本/图片不重定向。
    #[test]
    fn is_media_path_detects_audio_video_text() {
        assert!(is_media_path(Path::new("a.ogg")), "音频触发重定向");
        assert!(is_media_path(Path::new("b.MP3")), "大写扩展名大小写不敏感");
        assert!(!is_media_path(Path::new("c.txt")), "文本不重定向");
        assert!(!is_media_path(Path::new("d.png")), "图片不重定向");
        #[cfg(feature = "video")]
        assert!(is_media_path(Path::new("e.mp4")), "视频触发重定向（video feature）");
        #[cfg(feature = "video")]
        assert!(!is_media_path(Path::new("f.pdf")), "PDF 不重定向");
    }

    /// `record_media_window_size` 模型段：非媒体窗口不记录；音乐角色写音乐槽 + 护栏 +
    /// 去重。
    #[test]
    fn record_media_window_size_music_slot() {
        let mut m = AppModel::default();
        assert_eq!(
            record_media_window_size(&mut m, gpui::size(px(800.), px(600.))),
            None,
            "非媒体窗口不记录"
        );
        m.music_mode = true;
        let (kind, size) =
            record_media_window_size(&mut m, gpui::size(px(800.), px(600.))).unwrap();
        assert_eq!(kind, MediaWindowKind::Music);
        assert_eq!(size, gpui::size(px(800.), px(600.)));
        assert_eq!(m.music_window_size, Some(gpui::size(px(800.), px(600.))));
        // 同尺寸重复记录：无操作（不重复标记/写盘）。
        assert_eq!(
            record_media_window_size(&mut m, gpui::size(px(800.), px(600.))),
            None,
            "去重：同尺寸不重复记录"
        );
        // 最小尺寸护栏 clamp。
        let (_, size) = record_media_window_size(&mut m, gpui::size(px(10.), px(10.))).unwrap();
        assert_eq!(
            size,
            gpui::size(px(MIN_MEDIA_WIN_W), px(MIN_MEDIA_WIN_H)),
            "过小尺寸被 clamp 到护栏"
        );
    }
}
