//! PDF「阅读位置书签」持久化：`{ 规范化文件路径 → [(页码, 标题)] }`，JSON 存储在
//! 应用数据目录（与用户配置同目录的 `pdf_bookmarks.json`），原子落盘。
//!
//! - **按文件路径关联**：不同 PDF 各自一套书签，重开同一路径即恢复（Phase B3）。
//! - **纯函数 + 可测**：`to_json` / `from_json` / `normalize_key` 均可单测；
//!   读写走 `UserPrefs::config_path()` 父子目录——测试构建（config `test-mode`）自动
//!   落在隔离临时目录（K8），不触碰真实 `%APPDATA%`。
//! - **原子写**：复用 `base::util::atomic_write`（临时文件 + rename，hash-skip 不做——
//!   书签量小且只在变更时写，量级远低于会话/配置）。
//! - **纯逻辑与模型读写**：路径 / 序列化 / 落盘 / 缓存与模型读数 [`bookmarks_for`] /
//!   模型写数 [`add_bookmark`] / [`remove_bookmark`] 均在本 crate；宿主 `EditorApp` 只留
//!   薄适配（notify/toast 由宿主落地）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use config::user::UserPrefs;
use serde_json::{Value, json};
use session_store::path_key_json_store::{Cache, PathKeyJsonStore};
use shell_core::{AppModel, PdfBookmark};

/// 书签存储文件路径 = 配置父目录 / `pdf_bookmarks.json`（`%APPDATA%\vuick\`）。
pub fn bookmarks_path() -> PathBuf {
    match UserPrefs::config_path().parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join("pdf_bookmarks.json"),
        _ => PathBuf::from("pdf_bookmarks.json"),
    }
}

/// 规范化路径键：统一收口于 [`session_store::path_key_json_store::normalize_key`]
/// （canonicalize + 失败回退原样），与 `reading_progress::normalize_key` 规则逐字符一致。
/// `pub`：经 `shell::app` 重导出供 `tests/pdf.rs` 书签盘上读出断言（§8.6 外提清单）。
pub fn normalize_key(path: &Path) -> String {
    session_store::path_key_json_store::normalize_key(path)
}

/// 序列化为 JSON 对象（纯函数，供 [`save`] / 测试）。空 map 也落一个合法空对象。
pub fn to_json(map: &HashMap<String, Vec<PdfBookmark>>) -> Value {
    let mut obj = serde_json::Map::new();
    for (key, bms) in map {
        let arr: Vec<Value> = bms
            .iter()
            .map(|b| {
                json!({
                    "page": b.page_index,
                    "title": b.title,
                })
            })
            .collect();
        obj.insert(key.clone(), Value::Array(arr));
    }
    Value::Object(obj)
}

/// 从 JSON 值解析（纯函数）：字段缺省 / 类型不符 / 值非法一律跳过该条，绝不 panic。
pub fn from_json(v: &Value) -> HashMap<String, Vec<PdfBookmark>> {
    let mut out = HashMap::new();
    let Some(map) = v.as_object() else {
        return out;
    };
    for (key, arr) in map {
        let Some(arr) = arr.as_array() else {
            continue;
        };
        let mut list = Vec::new();
        for item in arr {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(page_index) = obj.get("page").and_then(Value::as_u64) else {
                continue;
            };
            let title = obj
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            // 缺标题（空串）的书签无展示意义 → 一并跳过（添加路径恒置「第 N 页」兜底）。
            if title.is_empty() {
                continue;
            }
            list.push(PdfBookmark {
                page_index: page_index as usize,
                title,
            });
        }
        out.insert(key.clone(), list);
    }
    out
}

/// 从磁盘加载书签存储（缺失 / 损坏 → 空 map）。
/// `pub`：经 `shell::app` 重导出供 `tests/pdf.rs` 书签盘上读出断言（§8.6 外提清单）。
pub fn load() -> HashMap<String, Vec<PdfBookmark>> {
    store().load()
}

/// A4 进程级书签缓存（路径守卫见 [`session_store::path_key_json_store::Cache`]）：每窗构造
/// 都读盘是浪费（构造链 `new_impl` → `bookmarks::load_cached()`），首次构造读一次、后续
/// 复用；`save` 同步回写缓存 → 进程内新窗口读到最新书签（本应用单进程，无外部改写，安全）。
static BOOKMARK_CACHE: Cache<HashMap<String, Vec<PdfBookmark>>> = OnceLock::new();

/// 绑定书签存根（路径 = `bookmarks_path()` + 本模块缓存 + 编解码），读写、缓存全部走共享层
/// （语义与旧实现逐字一致：缺失/损坏 → 空、写败静默、成功回写缓存）。
fn store() -> PathKeyJsonStore<HashMap<String, Vec<PdfBookmark>>> {
    PathKeyJsonStore::new(bookmarks_path(), &BOOKMARK_CACHE, from_json, to_json)
}

/// 取书签（进程级缓存一次读盘；`save` 更新缓存保证进程内一致）。
pub fn load_cached() -> HashMap<String, Vec<PdfBookmark>> {
    store().load_cached()
}

/// 原子落盘整份书签存储；A4：先写好盘面再回写缓存（缓存镜像盘面）。
pub fn save(map: &HashMap<String, Vec<PdfBookmark>>) {
    store().save(map);
}

/// 某路径当前内存态的书签列表（无 → `None`）。模型读数（宿主 `EditorApp` 薄适配转发；
/// 侧栏 TOC「书签」区段与测试读）。`Option<Vec<..>>`：`AppModel` 为 `Rc<RefCell>`、
/// 跨 crate 命名，owned 克隆语义与 `EditorApp::pdf_bookmarks_for` 一致。
pub fn bookmarks_for(m: &AppModel, path: &Path) -> Option<Vec<PdfBookmark>> {
    let key = normalize_key(path);
    m.pdf_bookmarks.get(&key).cloned()
}

/// 添加/覆盖一条书签（同页去重）：更新内存 + 原子落盘。宿主在模型借用结束后自行
/// notify（`EditorApp::add_pdf_bookmark` 的模型 + save 部分）。
///
/// **A4 多窗口安全**：不基于窗口本地快照 `m.pdf_bookmarks` 整份落盘——两窗并存时每窗
/// 启动各持一份 `load_cached()` 克隆（`shell/app/init.rs`），旧快照整份覆盖会抹掉对端
/// 窗口新增的书签。改为：以**最新盘面**（`store().load()` 直读磁盘、绕过窗口旧快照）
/// 为底，应用本次增量后整体落盘 + 回写缓存，再同步窗口内存态（模型读数一致）。
/// 单窗口语义不变：盘面 = 模型 = 缓存恒等。
pub fn add_bookmark(m: &mut AppModel, path: &Path, page_index: usize, title: String) {
    let key = normalize_key(path);
    let mut map = store().load();
    let list = map.entry(key.clone()).or_default();
    if let Some(existing) = list.iter_mut().find(|b| b.page_index == page_index) {
        existing.title = title;
    } else {
        list.push(PdfBookmark { page_index, title });
    }
    save(&map);
    m.pdf_bookmarks = map;
}

/// 移除某路径书签列表中第 `idx` 条（列表空 → 整键删除）+ 落盘；返回是否移除。
/// A4 多窗口安全同 [`add_bookmark`]：以最新盘面为底做增量，避免旧快照把对端已删书签
/// 「复活」回盘。
pub fn remove_bookmark(m: &mut AppModel, path: &Path, idx: usize) -> bool {
    let key = normalize_key(path);
    let mut map = store().load();
    let Some(list) = map.get_mut(&key) else {
        return false;
    };
    if idx >= list.len() {
        return false;
    }
    list.remove(idx);
    if list.is_empty() {
        map.remove(&key);
    }
    save(&map);
    m.pdf_bookmarks = map;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_json_roundtrip_and_bad_rows_skipped() {
        let mut map = HashMap::new();
        map.insert(
            "D:\\a\\b.pdf".to_string(),
            vec![
                PdfBookmark { page_index: 0, title: "封面".into() },
                PdfBookmark { page_index: 7, title: "第 8 页".into() },
            ],
        );
        map.insert("C:\\x.pdf".to_string(), vec![]);
        let json = to_json(&map);
        assert_eq!(from_json(&json), map, "往返应一致");

        // 坏行（page 非数字 / 缺 title / 非对象 / 非数组）一律跳过，不 panic。
        let bad = json!({
            "k1": [
                {"page": 2, "title": "ok"},
                {"page": "x", "title": "坏页码"},
                {"page": 3},
                "字符串",
            ],
            "k2": "非数组",
            "k3": 5,
        });
        let parsed = from_json(&bad);
        assert_eq!(parsed["k1"], vec![PdfBookmark { page_index: 2, title: "ok".into() }]);
        assert!(parsed.get("k2").is_none() && parsed.get("k3").is_none());

        // 空 / 非对象 → 空 map。
        assert!(from_json(&Value::Null).is_empty());
        assert!(from_json(&json!([])).is_empty());
    }

    /// 复现 P1「两窗并发写书签互相覆盖」：每窗 `AppModel` 启动各持一份 `load_cached()`
    /// 克隆快照（`shell/app/init.rs` `AppModel::new(::bookmarks::load_cached(), ...)`），
    /// 窗口 A 加书签 X 落盘后，窗口 B 若再增删书签，会用**不含 X 的旧快照整份覆盖**
    /// 落盘 —— `save(&m.pdf_bookmarks)` 落的是整份 map，X 被静默抹掉
    /// （注明「单进程无外部改写安全」只对单窗成立，两窗并存即破）。
    ///
    /// 预期（修复后）：B 用旧快照落盘不得抹掉 A 的书签 X，X 应仍在盘上。
    /// 当前实现（修复前）：B 的整体覆盖把 X 抹掉 → 本断言红。
    #[test]
    fn two_windows_add_bookmarks_with_stale_snapshot_does_not_lose_peer() {
        // 盘面与进程缓存复位到空 = 确定初态（同「全新安装/首次启动」）；config test-mode
        // 隔离目录，不触碰真实 %APPDATA%。
        save(&HashMap::new());

        // 两个窗口同时开着：各自构造时 `load_cached()` 得到同一份**空快照**（B 不重读盘面）。
        let mut window_a = AppModel::default();
        window_a.pdf_bookmarks = load_cached();
        let mut window_b = AppModel::default();
        window_b.pdf_bookmarks = load_cached();

        let pdf_a = Path::new("D:\\docs\\a.pdf");
        let pdf_b = Path::new("D:\\docs\\b.pdf");

        // 窗口 A 加书签 X → A 的整份 map 落盘（盘面含 X）。
        add_bookmark(&mut window_a, pdf_a, 3, "窗口A的书签X".to_string());
        // 窗口 B（旧快照不含 X）加书签 Y → B 的整份 map 覆盖落盘 → 修复前 X 被抹掉。
        add_bookmark(&mut window_b, pdf_b, 5, "窗口B的书签Y".to_string());

        // 断言 X 仍在盘上（盘面直读 `load()`，进程内缓存 `load_cached()` 应镜像一致）。
        let key_a = normalize_key(pdf_a);
        let disk = load();
        assert!(
            disk.get(&key_a).is_some(),
            "窗口 B（旧快照）整份覆盖落盘后，窗口 A 的书签 X 不应丢失；盘面 = {:?}",
            disk,
        );
        assert!(
            load_cached().get(&key_a).is_some(),
            "进程内缓存也应保留窗口 A 的书签 X（缓存镜像盘面）",
        );
    }

    /// remove 变体：`remove_bookmark` 同样 `save(&m.pdf_bookmarks)` 整份 map。—— 盘面预置
    /// A 的书签 X 后，两窗共享同快照；A 移除 X 落盘，B 再用**仍含 X 的旧快照**往另一 PDF
    /// 加书签 Y → B 的整份覆盖把已移除的 X「复活」回盘面（A 的移除被静默撤销）。
    ///
    /// 预期（修复后）：A 的移除生效，X 不应回到盘上。
    /// 当前实现（修复前）：X 被 B 的旧快照整体带回 → 本断言红。
    #[test]
    fn two_windows_remove_bookmark_with_stale_snapshot_does_not_undo_peer_delete() {
        // 确定初态：盘面预置 A 的书签 X（同「旧会话已读过 a.pdf 且点赞了第 3 页」）。
        let key_a = normalize_key(Path::new("D:\\docs\\a.pdf"));
        let key_b = normalize_key(Path::new("D:\\docs\\b.pdf"));
        save(&HashMap::from([(
            key_a.clone(),
            vec![PdfBookmark { page_index: 3, title: "X书签".to_string() }],
        )]));

        // 两窗同时开着：各自启动快照都含 X（缓存命中同一盘面）。
        let mut window_a = AppModel::default();
        window_a.pdf_bookmarks = load_cached();
        let mut window_b = AppModel::default();
        window_b.pdf_bookmarks = load_cached();

        // 窗口 A 移除 X 并落盘（盘面应已无 X）。
        assert!(
            remove_bookmark(&mut window_a, Path::new("D:\\docs\\a.pdf"), 0),
            "A 窗口应能移除自己的书签 X",
        );
        // 窗口 B（旧快照仍含 X）给另一 PDF 加书签 Y → B 的整份 map 覆盖落盘。
        add_bookmark(&mut window_b, Path::new("D:\\docs\\b.pdf"), 5, "Y书签".to_string());

        // 断言：A 已移除的 X 不应被 B 的整份覆盖「复活」回盘上。
        let disk = load();
        assert!(
            disk.get(&key_a).is_none(),
            "窗口 B（旧快照仍含 X）整份覆盖后，A 已移除的书签 X 不应复活回盘；盘面 = {:?}，B 的新增 = {:?}",
            disk,
            disk.get(&key_b),
        );
    }
}
