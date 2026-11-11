//! 预览域 crate。
//!
//! 把预览域的**统一 WyEditor（`md_wy`）安装 / 整篇装配 / 增量对齐 /
//! 活区（WYSIWYG）进入 / 编辑⇄预览滚动同步 / 路径分类 / 预览渲染元素**从 `EditorApp` 的
//! `impl` 块移交出来，使这些逻辑可独立编译 / 独立测试。宿主 `EditorApp` 只保留薄适配：
//!
//! - **模型字段**：`tabs[].md_wy` / `md_wy_vscroll` / `md_wy_install` / `live2` / `live_edit`
//!   / `live2_dirty` / `_live2_sub` / `live_vscroll` / `preview_on` / `preview_fullscreen` /
//!   `readonly_preview` / `epub_images` / `epub_notes` 与 `font_size` / `wrap_enabled` /
//!   `status` 等仍在 `shell_core::AppModel`（本 crate 直接消费，跨 crate 命名经 shell_core）。
//! - **宿主服务面**：活区编辑防抖持久化 / Ctrl+滚轮缩放字号 / 预览拖放打开 经泛型宿主
//!   [`host::PreviewHost`]
//!   反调宿主；域不直读 `EditorApp`。
//! - **留宿主的薄适配**：编辑事件**订阅与 feed 入口**（`app/sync.rs`：`subscribe_editor` /
//!   `on_editor_edited` 等，改经 `md_wy_feed_full_rope` / `preview_apply_edit` 调域）、
//!   `commit_live` 的宿主收尾（`on_next_frame` 清 `retired_live2` + 行级 diff 兜底 hunk 计算，
//!   `text_open::line_diff` 依赖宿主）、媒体/PDF 查看器渲染分支（`render_preview_pane` /
//!   `render_fullscreen_preview` 的 `md_wy_preview()==None` 半）、Menus/布局编排。
//!
//! ## 模块
//!
//! - [`classify`]：路径 → [`PathKind`] 分类（扩展名 → 类型 / 语法高亮 id / 状态栏 label，
//!   单一真值表）。
//! - [`mdwy`]：统一 `md_wy` WyEditor 预览的懒创建 / 喂源 / 增量对齐 / 整篇装配（含大文档
//!   后台异步装配与 Step2/3 提前解析换入）。
//! - [`live`]：WYSIWYG 活区（`live2`）进入（ON 分支：复用右栏已解析 Doc / 后台解析装配 /
//!   编辑日志收集）。
//! - [`scroll`]：编辑区 ⇄ 预览滚动同步（块起点键、Scrub 延后收敛状态机）。
//! - [`toggle`]：预览开关 / 全屏 / 换行的模型半（`activate_tab` 等宿主动作由宿主 wrapper 前置）。
//! - [`render`]：预览/WYSIWYG 渲染元素（`md_wy` 宿主、活区宿主、`zoom_overlay`、媒体查看器
//!   元素复用与自绘滚动条）。
//! - [`host`]：[`PreviewHost`] 宿主服务面。
//!
//! 依赖 workspace 既有包（gpui / ui / config / shell_core / markdown_wysiwyg /
//! text_open / image_viewer / ropey / video 可选）。

pub mod classify;
pub mod host;
pub mod live;
pub mod mdwy;
pub mod render;
pub mod scroll;
pub mod toggle;

pub use host::PreviewHost;
/// 测试探针根导出：B3 后台整篇装配发起次数 / toggle ON 共享复用右栏 Doc 次数——shell
/// 侧回归测试经 `app::preview::…`（宿主重导出）读取（常驻编译，见各定义处注释）。
pub use live::LIVE_ON_MD_WY_REUSE;
pub use mdwy::LIVE_COMMIT_MD_WY_ASYNC;
