//! PDF「阅读位置书签」持久化：`{ 规范化文件路径 → [(页码, 标题)] }`，JSON 存储在
//! 应用数据目录（与用户配置同目录的 `pdf_bookmarks.json`），原子落盘。
//!
//! - **按文件路径关联**：不同 PDF 各自一套书签，重开同一路径即恢复（Phase B3）。
//! - **纯函数 + 可测**：`to_json` / `from_json` / `normalize_key` 均可单测；
//!   读写走 `UserPrefs::config_path()` 父子目录——测试构建（config `test-mode`）自动
//!   落在隔离临时目录（K8），不触碰真实 `%APPDATA%`。
//! - **原子写**：复用 `base::util::atomic_write`（临时文件 + rename，hash-skip 不做——
//!   书签量小且只在变更时写，量级远低于会话/配置）。

use std::collections::HashMap;
use std::path::Path;

use super::*;
use config::user::UserPrefs;
use serde_json::{Value, json};

/// 一条阅读位置书签：目标页码（0 基）+ 展示标题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdfBookmark {
    /// 目标页码（0 基；`PdfViewer::scroll_to_page` 的直接入参）。
    pub(crate) page_index: usize,
    /// 展示标题（优先当前大纲标题；无则「第 N 页」，N 为 1 基页码）。
    pub(crate) title: String,
}

/// 书签存储文件路径 = 配置父目录 / `pdf_bookmarks.json`（`%APPDATA%\gpui-editor\`）。
pub(crate) fn bookmarks_path() -> std::path::PathBuf {
    match UserPrefs::config_path().parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join("pdf_bookmarks.json"),
        _ => std::path::PathBuf::from("pdf_bookmarks.json"),
    }
}

/// 规范化路径键：先 `canonicalize`（消 `.`/`..`、接真实盘符大小写形态），失败回退
/// 原样字符串。同一文件的后续打开 / 绝对化写法都落到同一键。
pub(crate) fn normalize_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// 序列化为 JSON 对象（纯函数，供 [`save`] / 测试）。空 map 也落一个合法空对象。
pub(crate) fn to_json(map: &HashMap<String, Vec<PdfBookmark>>) -> Value {
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
pub(crate) fn from_json(v: &Value) -> HashMap<String, Vec<PdfBookmark>> {
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
pub(crate) fn load() -> HashMap<String, Vec<PdfBookmark>> {
    let Ok(text) = std::fs::read_to_string(bookmarks_path()) else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    from_json(&v)
}

/// 原子落盘整份书签存储。
pub(crate) fn save(map: &HashMap<String, Vec<PdfBookmark>>) {
    let _ = base::util::atomic_write(bookmarks_path(), to_json(map).to_string().as_bytes());
}

impl EditorApp {
    /// 某路径当前内存态的书签列表（无 → `None`）。侧边栏 TOC「书签」区段与测试读。
    pub(crate) fn pdf_bookmarks_for(&self, path: &std::path::Path) -> Option<&[PdfBookmark]> {
        let key = normalize_key(path);
        self.pdf_bookmarks.get(&key).map(|v| v.as_slice())
    }

    /// 添加/覆盖一条书签（同页去重）：更新内存 + 原子落盘 + notify。
    pub(crate) fn add_pdf_bookmark(
        &mut self,
        path: &std::path::Path,
        page_index: usize,
        title: String,
        cx: &mut Context<Self>,
    ) {
        let key = normalize_key(path);
        let list = self.pdf_bookmarks.entry(key.clone()).or_default();
        if let Some(existing) = list.iter_mut().find(|b| b.page_index == page_index) {
            existing.title = title;
        } else {
            list.push(PdfBookmark { page_index, title });
        }
        save(&self.pdf_bookmarks);
        cx.notify();
    }

    /// 移除某路径书签列表中第 `idx` 条（列表空 → 整键删除）+ 落盘 + notify；返回是否移除。
    pub(crate) fn remove_pdf_bookmark(
        &mut self,
        path: &std::path::Path,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = normalize_key(path);
        let Some(list) = self.pdf_bookmarks.get_mut(&key) else {
            return false;
        };
        if idx >= list.len() {
            return false;
        }
        list.remove(idx);
        if list.is_empty() {
            self.pdf_bookmarks.remove(&key);
        }
        save(&self.pdf_bookmarks);
        cx.notify();
        true
    }

    /// PDF 右键「添加书签」动作：当前页 + 标题（优先当前大纲标题；无则「第 N 页」）。
    pub(crate) fn pdf_add_bookmark_action(
        &mut self,
        pdf: &gpui::Entity<pdf::PdfViewer>,
        cx: &mut Context<Self>,
    ) {
        let Some((page1, _)) = pdf.read(cx).current_page() else {
            return;
        };
        let page_index = page1.saturating_sub(1);
        let title = pdf
            .read(cx)
            .outlines()
            .iter()
            .find(|o| o.page_index == page_index)
            .map(|o| o.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("第 {page1} 页"));
        let path = pdf.read(cx).path().to_path_buf();
        self.add_pdf_bookmark(&path, page_index, title.clone(), cx);
        self.pdf_toast("已添加书签", format!("第 {page1} 页 · {title}"));
    }
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
}
