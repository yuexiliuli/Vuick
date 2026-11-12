//! 会话持久化 + 设置导出 + 最近面板实体 叶子 crate（2026-11 自 `crates/shell` 拆出）。
//!
//! 纯结构移动（无函数体 / 算法 / 行为 / 接口语义改动），消费方的 **编排（`impl EditorApp`）
//! 留在 shell**，本 crate 只交付三块可独立编译 / 独立测试的存储与序列化层：
//!
//! - [`session`]：会话临时文件存储层——清单（`session/session.json`）+ 未命名槽位
//!   （`t0..tN`）的读 / 写 / 清 / 导入 / 孤儿清理，全部显式目录驱动、无 host 依赖。
//! - [`settings_xfer`]：设置导出包（配置 + 会话全量打包）的构建 / 校验 / 解析纯函数。
//! - [`recent_panel`]：独立 `RecentPanel` 实体（Acrylic 居中大面板），经最小宿主接口
//!   [`RecentPanelHost`] 反调 shell 宿主，不直接依赖任何 shell 类型。
//!
//! 依赖只重分配 workspace 既有包：`gpui` / `ui` / `base` / `serde_json`。

pub mod reading_progress;
pub mod recent_panel;
pub mod session;
pub mod settings_xfer;

pub use reading_progress::{ReadingProgress, from_json, load, load_cached, normalize_key, save, to_json};
pub use recent_panel::{RecentMode, RecentPanel, RecentPanelHost};
pub use session::{
    MAX_IMPORT_SLOT_BYTES, SessionManifest, SessionTab, clear_session, prune_unreferenced_slots,
    read_manifest, read_slot, write_imported_session, write_manifest, write_slot,
};
pub use settings_xfer::{ParsedExportBundle, build_export_bundle, parse_export_bundle};
