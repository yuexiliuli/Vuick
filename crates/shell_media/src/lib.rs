//! 媒体/音乐域 crate（2026-11-11 自 `crates/shell` 的 `app/media` + `app/media_window` +
//! `app/music_player` 拆出）。
//!
//! 纯结构移动 + 域内解耦：把媒体域的**查看器元数据同步 / 锁定菜单项构建 / 媒体窗口打开
//! 决策与路由决议 / 音乐模式判定**从 `EditorApp` 的 `impl` 块整迁出来，使这些逻辑可独立
//! 编译 / 独立测试。宿主 `EditorApp` 只保留薄适配：
//!
//! - **模型字段**：`media_new_window` / `reuse_media_window` / `music_window_size` /
//!   `video_window_size` / `music_mode` 等仍在 `shell_core::AppModel`（跨 crate 命名由
//!   shell_core 提供，本 crate 直接消费）。
//! - **宿主服务面**：媒体窗口路由的「真正执行」（新开窗口 / 跨窗口打开）与「最近其它
//!   媒体窗口」查询经 [`host::MediaHost`]（对照 `shell_pdf::PdfHost` 先例）反调宿主；
//!   域不直读 `EditorApp` / `EditorAppRegistry`。
//! - **留宿主的薄适配**：右键菜单的**视图/挂载**（`ui::menu` 弹出结构 + `media_lock_menu_*`
//!   字段）、媒体窗口 `open_window` 装配、音乐 `MusicView` 惰性创建与焦点编排留 shell。
//!
//! ## 模块
//!
//! - [`media`]：图片/视频查看器元数据同步、锁定判定、右键「锁定/解锁」菜单项与浮层构建。
//! - [`media_window`]：媒体窗口打开**纯决策**（`MediaOpenTarget` / `MediaWindowKind` /
//!   `resolve_*` / `media_window_kind` / 尺寸记录模型段 / 路由动作决议）。
//! - [`music`]：音乐模式路由判定（什么路径进音乐模式）、toggle 状态判定与播放请求核心。
//! - [`host`]：[`MediaHost`] 宿主服务面（路由执行 / 最近媒体窗口查询）。
//!
//! 依赖只重分配 workspace 既有包（gpui / ui / shell_core / music / video 可选——
//! `image_viewer` 死依赖已随评审 nit 移除，`MediaReader::Image` 实体仅经 shell_core 命名），
//! 不新增外部包。

pub mod host;
pub mod media;
pub mod media_window;
pub mod music;

pub use host::MediaHost;
pub use media_window::{MediaOpenAction, MediaOpenTarget, MediaWindowKind};
