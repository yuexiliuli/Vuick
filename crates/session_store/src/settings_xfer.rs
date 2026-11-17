//! 设置导出包（配置 + 会话**全量**打包为单个 `.json`）的**纯序列化层**（自
//! `crates/shell/src/app/settings_xfer.rs` 纯段迁出，2026-11）：构建 / 校验 / 解析，
//! 自含魔力头与版本可校验——「搬到别处导入也还在」。**应用层**（对话框 / 配置即时生效 /
//! 会话写回目录 / toast，即 `EditorApp::apply_import_bundle` 等）留在 shell 的
//! `app::settings_xfer`。
//!
//! 打包格式（单个 `.json`）：
//! ```json
//! { "app": "vuick", "export_version": 1,
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
const EXPORT_MAGIC: &str = "vuick";
/// 兼容旧版（gpui-editor 时代）导出的魔力头：仅导入放行，导出永远写 [`EXPORT_MAGIC`]
/// 新名——旧文件「搬到别处导入也还在」不因改名失效。
const LEGACY_EXPORT_MAGIC: &str = "gpui-editor";
/// 导出包格式版本（自含版本可校验；未来格式演进用版本号迁移）。
const EXPORT_VERSION: u64 = 1;

/// 导入包**总字节上限**：整个导入包（prefs + 会话清单 + 槽位合计）不得超过
/// [`crate::MAX_IMPORT_SLOT_BYTES`]（64 MiB，与写回盘侧**单槽位**上限一致）。
/// 超限 → 解析期（`serde_json::from_str` 物化 Value 之前）直接拒，不物化超大 Value。
/// `pub`：复现红测与调用方按边界断言。
///
/// **口径 = JSON 编码后字节**（非解码合计）：闸的本职是「喂给 `from_str` 的原始输入长度
/// 封顶」，故按编码长度判；JSON 转义只放大不缩小（编码 ≥ 解码），因此不会放行解码超限的
/// 槽位（逐槽复核再保单槽 ≤ 单槽上限，与写回盘侧对称）；仅在「解码近上限 + 编码略放大」
/// 的绝对边界会保守多拒一次（安全方向，正常/小包不受影响）。
pub const IMPORT_MAX_BYTES: usize = crate::MAX_IMPORT_SLOT_BYTES;

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
    // 总量闸：整个导入包（prefs + 会话清单 + 槽位合计）字节数超上限 → 解析期直接拒。
    // 必须放在 serde_json::from_str 物化 Value **之前**按原始字节长度判断——恶意/退化
    // 超大包若先物化，Value 树 + 每槽位 to_string 克隆会在解析期即撑爆内存（写回盘的
    // [`crate::MAX_IMPORT_SLOT_BYTES`] 检查要到解析完成后才走到）。
    // 总量闸：整个导入包（prefs + 会话清单 + 槽位合计）字节数超上限 → 解析期直接拒。
    // 必须放在 serde_json::from_str 物化 Value **之前**按原始字节长度判断——恶意/退化
    // 超大包若先物化，Value 树 + 每槽位 to_string 克隆会在解析期即撑爆内存（写回盘的
    // [`crate::MAX_IMPORT_SLOT_BYTES`] 检查要到解析完成后才走到）。
    if text.len() > IMPORT_MAX_BYTES {
        return Err(());
    }
    let v: Value = serde_json::from_str(text).map_err(|_| ())?;
    let Some(obj) = v.as_object() else {
        return Err(());
    };
    // 新名 + 旧名双放行（改名兼容：VUICK 时代新旧导出文件都能导入）。
    let app = obj.get("app").and_then(|x| x.as_str());
    if app != Some(EXPORT_MAGIC) && app != Some(LEGACY_EXPORT_MAGIC) {
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
    // 逐槽复核（对齐写回盘侧语义）：任一槽位正文超过单槽位上限 → 整个导入包拒绝
    // （同 [`crate::write_imported_session`] 的「拒整个会话、零写入」语义）。与总量闸双保险：
    // 总量闸挡「整包超限」，本条挡「总量未超但单槽超限 / 海量小键绕过」的把关，均不改
    // JSON 结构语义（非字符串槽位值仍按既有的 filter_map 跳过）。
    if let Some((_, slots)) = &session {
        if slots
            .iter()
            .any(|(_, content)| content.len() > crate::MAX_IMPORT_SLOT_BYTES)
        {
            return Err(());
        }
    }
    Ok(ParsedExportBundle { prefs, session_present, session })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个结构合法（magic/版本/清单/槽位齐全）的导出包 JSON 文本；
    /// `slots` 为 `(槽位名, 正文)` 注入列表——red 用例需多槽位「单槽不超限、合计超限」，
    /// 故按 `build_export_bundle` 同签名接受列表。
    /// 手拼 `format!` 而非 `serde_json::json!`：red 用例的槽位正文是 64 MiB 级，
    /// `json!` 会对超大字符串多做整串克隆（Value + to_string 两份），省内存与测试时间。
    fn bundle_json(slots: &[(&str, &str)]) -> String {
        // 槽位正文仅 'x'，无需 JSON 字符串转义；手拼结构等价 build_export_bundle 的产物。
        let inner = slots
            .iter()
            .map(|(name, content)| format!(r#""{name}":"{content}""#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"app":"vuick","export_version":1,"prefs":{{}},"session":{{"manifest":{{"version":1,"active_tab":0,"tabs":[]}},"slots":{{{inner}}}}}}}"#
        )
    }

    /// 护栏（应绿）：与 red 用例同一构造路径、仅槽位大小不同——证明 red 用例的差异点就是
    /// 「总字节超限」这个正在测的防线，而非手拼 JSON 本身格式错误造成的假红/假绿（K173）。
    #[test]
    fn parse_export_bundle_accepts_small_valid_bundle() {
        let text = bundle_json(&[("t0", "小卡片正文")]);
        let parsed = parse_export_bundle(&text);
        assert!(parsed.is_ok(), "合法小包必须解析成功");
        let parsed = parsed.unwrap();
        assert!(parsed.session_present, "含 session 段");
        let (manifest, slots) = parsed.session.expect("session 段应可解析");
        assert_eq!(manifest.version, 1);
        assert_eq!(slots.len(), 1, "单槽位");
        assert_eq!(slots[0].0, "t0");
        assert_eq!(slots[0].1, "小卡片正文", "槽位正文原样带回");
    }

    /// M「导入解析无总量上界」的 red 测：导入包总字节超上限必须被拒绝。
    ///
    /// 构造必须**隔离总量闸**：两槽位各 ⅔×64 MiB——每槽单独 ≤
    /// [`crate::MAX_IMPORT_SLOT_BYTES`]（逐槽复核放行）、合计 ≈1.33×64 MiB 只有总量闸能拦。
    /// 用「单槽 64MiB+1」构造得不出这点：同包会先被逐槽复核拦住，剔掉总量闸也仍 Err → 假绿，
    /// 测不到正在测的防线（K173：剔除总量闸必须变红才算真测到目标）。
    ///
    /// 期望语义（实现将补）：引入总包上限常量 `IMPORT_MAX_BYTES`（建议 == 单槽位上限
    /// [`crate::MAX_IMPORT_SLOT_BYTES`] = 64 MiB，同一导出包的 prefs/清单/槽位合计
    /// 不得超过它），在 `serde_json::from_str` 物化 Value **之前**按 `text.len()` 直接
    /// 拒绝——防恶意/退化包「超大/海量键解析期已物化 + 逐槽位 `to_string` 全量克隆」
    /// 撑爆内存（写回盘的 [`crate::MAX_IMPORT_SLOT_BYTES`] 检查只在解析完成后才到）。
    #[test]
    fn parse_export_bundle_must_reject_total_oversize_bundle() {
        let slot_body = "x".repeat(crate::MAX_IMPORT_SLOT_BYTES * 2 / 3);
        let oversized = bundle_json(&[("t0", &slot_body), ("t1", &slot_body)]);
        assert!(
            oversized.len() > crate::MAX_IMPORT_SLOT_BYTES,
            "payload 必须构成「总包超限」（当前仅约 +0B 边界，构造有误）"
        );
        assert!(
            parse_export_bundle(&oversized).is_err(),
            "超总包上限的导入必须被拒绝（当前无总量上限 → 逐槽复核放行 → 照常解析为 Ok）"
        );
    }
}
