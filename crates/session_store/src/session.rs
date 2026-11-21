//! 会话临时文件存储层（自 `crates/shell/src/app/session.rs` 纯存储段迁出，2026-11）：
//! 清单 + 槽位文件的读 / 写 / 清 / 导入 / 孤儿清理纯逻辑，全部显式 `Path` 目录驱动、
//! 无 host 依赖，可独立复用与测试。**编排**（`EditorApp` 的收集 / 渐进恢复 / 开关 /
//! 退出钩子）留在 shell 的 `app::session`。
//!
//! 储存格式：
//! - 清单 `session/session.json`：`{"version":1, "active_tab":N, "tabs":[
//!   {"kind":"file","path":"<绝对路径>"}, {"kind":"new","slot":"t0","name":"未命名"}]}`
//! - 未命名选项卡内容写槽位文件 `session/t0`、`t1`…（LF 规范化，空内容也写空文件）。
//! - 清单 + 槽位一律「临时文件 + rename」原子写；损坏 / 缺字段一律安全回退（不 panic）。
//!
//! **导入安全（K42 输入安全清单）**：[`write_imported_session`] 是导入包的**唯一下盘口**，
//! 写任何槽位前先整体预检——每个槽位名过 [`safe_slot_name`]（防路径穿越 / 越界任意写盘）
//! + 正文大小上限 [`MAX_IMPORT_SLOT_BYTES`]；清单引用的槽位名同样受检。任一不合格 →
//!
//! 整体拒绝、零写入（不留半个会话）。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 清单里单个会话选项卡。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTab {
    /// 既有文件：下次启动按路径重新打开。
    File { path: PathBuf },
    /// 未命名选项卡：内容存槽位文件（`session/t0`…）；`name` 仅作清单可读性保留
    /// （EditorTab 无名字段，恢复后仍显示常量「未命名」）。
    New { slot: String, name: String },
}

/// 会话清单。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SessionManifest {
    /// 格式版本（当前 1；未知版本一律回退默认，不 panic）。
    pub version: u32,
    /// 活动选项卡下标（恢复时 clamp 到合法范围）。
    pub active_tab: usize,
    /// 按顺序的选项卡列表（顺序即恢复顺序）。
    pub tabs: Vec<SessionTab>,
}

impl SessionTab {
    fn to_value(&self) -> Value {
        match self {
            SessionTab::File { path } => json!({ "kind": "file", "path": path.to_string_lossy() }),
            SessionTab::New { slot, name } => json!({ "kind": "new", "slot": slot, "name": name }),
        }
    }

    /// 解析选项卡元素：字段缺 / 类型不符 → None（安全回退整项）。
    fn from_value(v: &Value) -> Option<SessionTab> {
        let obj = v.as_object()?;
        match obj.get("kind")?.as_str()? {
            "file" => {
                let path = PathBuf::from(obj.get("path")?.as_str()?);
                Some(SessionTab::File { path })
            }
            "new" => Some(SessionTab::New {
                slot: obj.get("slot")?.as_str()?.to_string(),
                name: obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("未命名")
                    .to_string(),
            }),
            _ => None,
        }
    }
}

impl SessionManifest {
    /// 序列化成 JSON 对象。`pub`：除 [`read_manifest`] 外，「导出设置」
    /// （[`super::settings_xfer::build_export_bundle`]）与 shell 的清单指纹跳过
    /// （`manifest.to_value().to_string()`）都复用同一序列化口径，避免复制清单格式。
    pub fn to_value(&self) -> Value {
        json!({
            "version": self.version,
            "active_tab": self.active_tab,
            "tabs": self.tabs.iter().map(SessionTab::to_value).collect::<Vec<_>>(),
        })
    }

    /// 清单回退语义：非对象 / 缺字段 / 损坏 → 最小可用清单（不 panic）。
    /// `pub(crate)`：`read_manifest` 与「导入设置」的 [`super::settings_xfer::parse_export_bundle`]
    /// 从导出包解析清单共用。
    pub(crate) fn from_value(v: &Value) -> SessionManifest {
        let Some(obj) = v.as_object() else {
            return Self::default();
        };
        let version = obj
            .get("version")
            .and_then(|x| x.as_u64())
            .unwrap_or(1)
            .min(u32::MAX as u64) as u32;
        let active_tab = obj.get("active_tab").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let tabs = obj
            .get("tabs")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(SessionTab::from_value).collect())
            .unwrap_or_default();
        SessionManifest {
            version,
            active_tab,
            tabs,
        }
    }
}

/// 清单文件路径。
fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

/// 槽位文件路径（`t0`、`t1`…）。
fn slot_path(dir: &Path, slot: &str) -> PathBuf {
    dir.join(slot)
}

/// 「临时文件 + rename」原子写（实现统一收敛到 `base::util::atomic_write`，此处只保留
/// 调用方的布尔化错误信号；Windows `std::fs::rename` 可覆盖既有文件，不再先删旧文件）。
fn atomic_write(path: &Path, body: &str) -> bool {
    if path.parent().is_none() {
        return false;
    }
    base::util::atomic_write(path, body.as_bytes()).is_ok()
}

/// 写清单（槽位全部写成功后调用——清单存在即「本次会话可恢复」的原子信号）。
/// `pub`：随迁出供 shell 编排 / 测试直接驱动显式临时目录。
pub fn write_manifest(dir: &Path, m: &SessionManifest) -> bool {
    atomic_write(&manifest_path(dir), &m.to_value().to_string())
}

/// 读清单：文件缺失 / JSON 损坏 / 非对象 → None（不可恢复）。
pub fn read_manifest(dir: &Path) -> Option<SessionManifest> {
    let text = fs::read_to_string(manifest_path(dir)).ok()?;
    let v = serde_json::from_str::<Value>(&text).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(SessionManifest::from_value(&v))
}

/// 写槽位内容（LF 规范化；空内容也写空文件）。`pub`：shell 编排与测试直接驱动。
pub fn write_slot(dir: &Path, slot: &str, content: &str) -> bool {
    // 快速路径：无 \r → 内容直接原子写（编辑内容内部已 LF，绝大多数情况命中，省两次整串拷贝）。
    if !content.as_bytes().contains(&b'\r') {
        return atomic_write(&slot_path(dir, slot), content);
    }
    let lf = content.replace("\r\n", "\n").replace('\r', "\n");
    atomic_write(&slot_path(dir, slot), &lf)
}

/// 读槽位内容（缺失 → None）。`pub`：shell 编排与测试直接驱动。
///
/// 安全：槽位名过 [`safe_slot_name`] 校验——清单是**不可信输入**，若被篡改为
/// `..\victim.txt` 等越界名，`dir.join(slot)` 会读到会话目录之外的文件；非合法槽位名
/// 一律视为缺失（None），杜绝穿越读取。
pub fn read_slot(dir: &Path, slot: &str) -> Option<String> {
    if !safe_slot_name(slot) {
        return None;
    }
    fs::read_to_string(slot_path(dir, slot)).ok()
}

/// 清会话：删清单 + 尽量清掉清单引用的槽位文件（幂等；单项失败不影响其余）。
/// `pub`：开关关闭与恢复完成共用，测试直接驱动。
///
/// 安全：清单引用槽位时先过 [`safe_slot_name`] 校验——清单是**不可信输入**，篡改的
/// 槽位名若不校验就 `remove_file` 会删除会话目录之外的任意文件（穿越删除）；非法槽位
/// 名跳过（幂等），只清合法槽位与清单本体。
pub fn clear_session(dir: &Path) -> bool {
    if let Some(m) = read_manifest(dir) {
        for tab in &m.tabs {
            if let SessionTab::New { slot, .. } = tab
                && safe_slot_name(slot) {
                    let _ = fs::remove_file(slot_path(dir, slot));
                }
        }
    }
    let _ = fs::remove_file(manifest_path(dir));
    true
}

/// 导入槽位正文大小上限（字节）：防护恶意/退化导出包用超大槽位撑爆磁盘或内存。
/// 64 MiB 对正常未命名大文档富余；超限 → 拒掉**整个**会话（零写入，不半途落盘）。
/// `pub`：测试直接按此边界断言（+1 拒、边界内收）。
pub const MAX_IMPORT_SLOT_BYTES: usize = 64 * 1024 * 1024;

/// 槽位名必须是「单一文件名」：非空、长度有界、不含任何路径分隔符 / 盘符前导 /
/// NUL（`/`、`\`、`:` 全拒——`../x`、`C:\…`、`c:foo` 盘符相对等路径穿越/越界一律
/// 进不来），非 `.`/`..`，非绝对路径，且不占用清单名 `session.json`。导入包是**不可信
/// 输入**，`dir.join(slot)` 未经此校验会把槽位写出 `session` 目录任意位置（任意写盘）；
/// 本应用自产槽位恒为 `t0..tN`，天然满足，不破坏 roundtrip。
fn safe_slot_name(slot: &str) -> bool {
    !slot.is_empty()
        && slot.len() <= 256
        && !slot.contains('/')
        && !slot.contains('\\')
        && !slot.contains(':')
        && !slot.contains('\0')
        && slot != "."
        && slot != ".."
        && slot != "session.json"
        && !Path::new(slot).is_absolute()
}

/// 把「导入设置」的会话数据写回会话目录（下一启动恢复）：先建目录、再各槽位内容、
/// 最后清单（清单 = 可恢复的原子信号，与 [`write_manifest`] / shell 的
/// `save_session_to` 同一信号语义）。槽位缺失/写失败 → false（清单不写，不留半个会话）。
///
/// **安全**：写任何槽位前先整体预检——每个槽位名过 [`safe_slot_name`]（防路径穿越 /
/// 越界任意写盘）+ 正文大小上限；清单引用的槽位名同样受检（否则可用 `slots` 之外的受控
/// 清单绕过）。任一不合格 → 整体拒绝、零写入（不留半个会话）。`pub`：shell 的
/// `apply_import_bundle` 与测试直接驱动。
pub fn write_imported_session(
    dir: &Path,
    manifest: &SessionManifest,
    slots: &[(String, String)],
) -> bool {
    for (slot, content) in slots {
        if !safe_slot_name(slot) || content.len() > MAX_IMPORT_SLOT_BYTES {
            return false;
        }
    }
    if manifest.tabs.iter().any(|t| match t {
        SessionTab::New { slot, .. } => !safe_slot_name(slot),
        _ => false,
    }) {
        return false;
    }
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    for (slot, content) in slots {
        if !write_slot(dir, slot, content) {
            return false;
        }
    }
    write_manifest(dir, manifest)
}

/// 清理孤儿槽位（纯函数，可测）：删除 `dir` 内**不是** `session.json`、**不是**
/// `manifest.tabs[].New.slot` 引用、且**非 `.tmp` 后缀**的文件（即陈旧 tN 槽位）。
/// `remove_tab` 后即时持久化会重编号未命名槽位，被关选项卡的旧槽位若不复用即成
/// 孤儿——本函数保证「否→删除临时文件」真正落盘 + 根治孤儿泄漏。`dir` 不存在 /
/// 读不出 → 安全返回 true（不 panic、无操作）。`pub`：shell 编排与测试直接驱动。
pub fn prune_unreferenced_slots(dir: &Path, manifest: &SessionManifest) -> bool {
    let mut referenced: HashSet<&str> = HashSet::new();
    for tab in &manifest.tabs {
        if let SessionTab::New { slot, .. } = tab {
            referenced.insert(slot.as_str());
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "session.json" || name.ends_with(".tmp") || referenced.contains(name.as_str()) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 进程内唯一临时父目录（每次用例独立盘面，互不串扰；同批用例也不互踩）。
    fn tmp_parent(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vuick-session-test-{tag}-{nanos}"))
    }

    #[test]
    fn clear_session_must_not_delete_outside_via_manifest_slot_traversal() {
        // 布局：父目录 P 内含「会话目录 P/session」与「受害者 P/victim.txt」（会话目录之外）。
        let parent = tmp_parent("clear");
        let s = parent.join("session");
        std::fs::create_dir_all(&s).unwrap();
        let victim = parent.join("victim.txt");
        std::fs::write(&victim, "VICTIM-CONTENT").unwrap();

        // 篡改清单：未命名选项卡槽位名写成「上一级目录穿越」。read_manifest 读的是不可信
        // session.json，修复前 clear_session 对 `dir.join("..\\victim.txt")` 直接
        // remove_file —— 会删掉会话目录之外的受害者文件。
        let crafted = SessionManifest {
            version: 1,
            active_tab: 0,
            tabs: vec![SessionTab::New {
                slot: "..\\victim.txt".into(),
                name: "x".into(),
            }],
        };
        assert!(write_manifest(&s, &crafted), "应能写入篡改清单");

        clear_session(&s);

        assert!(
            victim.exists(),
            "越界槽位名不得删除会话目录之外的任何文件（穿越删除）"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn read_slot_must_not_read_via_slot_traversal() {
        let parent = tmp_parent("read");
        let s = parent.join("session");
        std::fs::create_dir_all(&s).unwrap();
        let victim = parent.join("victim.txt");
        std::fs::write(&victim, "VICTIM-CONTENT").unwrap();

        // 越界槽位名（上一级目录穿越）。read_slot 的语义是「缺失 → None」；修复前
        // `dir.join("..\\victim.txt")` 会真读到会话目录之外的受害者内容 —— 必须视为缺失。
        assert_eq!(
            read_slot(&s, "..\\victim.txt"),
            None,
            "越界槽位名必须视为缺失（None），不得读取会话目录之外的任何文件"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }
}
