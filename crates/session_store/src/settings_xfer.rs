//! 设置导出包（配置 + 会话**全量**打包为单个 `.json`）的**纯序列化层**（自
//! `crates/shell/src/app/settings_xfer.rs` 纯段迁出，2026-11）：构建 / 校验 / 解析，
//! 自含魔力头与版本可校验——「搬到别处导入也还在」。**应用层**（对话框 / 配置即时生效 /
//! 会话写回目录 / toast，即 `EditorApp::apply_import_bundle` 等）留在 shell 的
//! `app::settings_xfer`。
//!
//! 打包格式（单个 `.json`）：
//! ```json
//! { "app": "gpui-editor", "export_version": 1,
//!   "prefs": { ...UserPrefs 全字段... },
//!   "session": { "manifest": { "version":1, "active_tab":N, "tabs":[...] },
//!                "slots": { "t0": "<正文>", ... } } }
//! ```
//! - prefs 复用 `UserPrefs::to_json_value`（shell 侧 `collect_prefs` 与持久化同一口径）；
//! - session 复用 [`super::session`] 的清单序列化（[`super::SessionManifest::to_value`]），
//!   各槽位正文经 serde_json 字符串转义嵌入。

use serde_json::{json, Map, Value};

use crate::SessionManifest;

/// 导出包魔力头（非本应用文件 → 拒绝导入）。
const EXPORT_MAGIC: &str = "gpui-editor";
/// 导出包格式版本（自含版本可校验；未来格式演进用版本号迁移）。
const EXPORT_VERSION: u64 = 1;

/// 解析后的导出包：prefs 与 session 各自独立可选——「部分可解析」据此实现
/// （prefs 有效但 session 损坏 → 配置照常应用 + toast 说明跳过，不整体回滚）。
pub struct ParsedExportBundle {
    pub prefs: Option<Value>,
    /// 导出包是否**含** `session` 段（区别于「有段但损坏」：缺段 = 无会话可导，
    /// 有段损坏 = 跳过，两者 toast 文案不同）。
    pub session_present: bool,
    pub session: Option<(SessionManifest, Vec<(String, String)>)>,
}

/// 构建导出包 JSON 字符串（纯函数，可测）：magic + 版本 + prefs 全字段 + 会话清单 +
/// 各槽位正文（槽位内容经 serde_json 字符串转义嵌入，无双引号/换行串通冲突）。
pub fn build_export_bundle(
    prefs: &Value,
    manifest: &SessionManifest,
    slots: &[(String, String)],
) -> String {
    let mut slots_obj = Map::new();
    for (slot, content) in slots {
        slots_obj.insert(slot.clone(), Value::String(content.clone()));
    }
    json!({
        "app": EXPORT_MAGIC,
        "export_version": EXPORT_VERSION,
        "prefs": prefs,
        "session": {
            "manifest": manifest.to_value(),
            "slots": slots_obj,
        },
    })
    .to_string()
}

/// 校验并解析导出包：JSON 损坏 / 非对象 / 魔力头不符 / 版本不符 → `Err`（调用方必须
/// 零副作用 toast「不是有效…」）；合法 → prefs（`"prefs"` 须为对象，否则 `None`）、
/// session（`"session"` 须为含 `"manifest"` 对象的对象，否则 `None`）。
pub fn parse_export_bundle(text: &str) -> Result<ParsedExportBundle, ()> {
    let v: Value = serde_json::from_str(text).map_err(|_| ())?;
    let Some(obj) = v.as_object() else {
        return Err(());
    };
    if obj.get("app").and_then(|x| x.as_str()) != Some(EXPORT_MAGIC) {
        return Err(());
    }
    if obj.get("export_version").and_then(|x| x.as_u64()) != Some(EXPORT_VERSION) {
        return Err(());
    }
    let prefs = obj.get("prefs").filter(|p| p.is_object()).cloned();
    let session_present = obj.contains_key("session");
    let session = obj
        .get("session")
        .and_then(|s| s.as_object())
        .and_then(|sobj| {
            let m = sobj.get("manifest").filter(|m| m.is_object())?;
            let manifest = SessionManifest::from_value(m);
            let slots = sobj
                .get("slots")
                .and_then(|s| s.as_object())
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some((manifest, slots))
        });
    Ok(ParsedExportBundle { prefs, session_present, session })
}
