//! 「阅读进度恢复」持久化存储层：`{ 规范化文件路径 → ReadingProgress }`，独立 JSON 文件
//! （文件名由调用方注入：默认 `%APPDATA%\gpui-editor\reading_progress.json`，测试走隔离
//! 临时目录），原子落盘。2026-11 新增（用户感知线：关 tab / 崩溃后重开回到上次阅读位置）。
//!
//! - **按文件路径关联**：不同文件各自一套进度，重开同一路径即恢复（镜像
//!   `shell_pdf/src/bookmarks.rs` 姿态）。
//! - **纯序列化叶子**：本模块**不依赖 config**——存储路径由调用方（shell_core / shell
//!   宿主）从 `UserPrefs::config_path()` 父目录算好经参数传入，可独立复用与测试。
//! - **原子写**：复用 `base::util::atomic_write`（临时文件 + rename，失败静默回退）。
//! - **A4 进程级缓存**：`OnceLock<Mutex<Option<(PathBuf, map)>>>` 镜像 bookmarks 姿势
//!   （每窗构造不重复读盘）；缓存条目带路径守卫——路径变化自动重读，进程内各测试临时
//!   目录互不串。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

use serde_json::{Value, json};

/// 一种文件类型的「阅读进度」（保存源行 / 页；重开时恢复到该位置）。
#[derive(Clone, Debug, PartialEq)]
pub enum ReadingProgress {
    /// Markdown / 纯文本：保存「视口顶源码行」（0 基；md 与 txt 都用行定位）。
    Markdown { top_line: usize },
    /// EPUB：保存「视口顶源码行」（0 基，来源 WyEditor Doc 的 src_line）。
    Epub { top_line: usize },
    /// PDF：保存「页下标 + 页内比例 + 缩放倍率」。
    Pdf {
        page_index: usize,
        in_page_frac: f32,
        zoom: f32,
    },
}

impl ReadingProgress {
    /// 序列化为 JSON 对象（纯函数；`pdf` 分支数值须有限且比例在 [0,1]）。
    fn to_value(&self) -> Value {
        match self {
            ReadingProgress::Markdown { top_line } => {
                json!({ "kind": "markdown", "top_line": top_line })
            }
            ReadingProgress::Epub { top_line } => {
                json!({ "kind": "epub", "top_line": top_line })
            }
            ReadingProgress::Pdf { page_index, in_page_frac, zoom } => json!({
                "kind": "pdf",
                "page_index": page_index,
                "in_page_frac": in_page_frac,
                "zoom": zoom,
            }),
        }
    }

    /// 从 JSON 值解析：字段缺省 / 类型不符 / 数值非法一律返回 `None`（该条跳过，
    /// 绝不 panic）。PDF 分支额外校验：`in_page_frac ∈ [0,1]` 且数值有限（NaN/∞/越界
    /// 视为损坏 → 整条跳过，防恢复出病态滚动位）。
    fn from_value(v: &Value) -> Option<ReadingProgress> {
        let obj = v.as_object()?;
        match obj.get("kind")?.as_str()? {
            "markdown" => Some(ReadingProgress::Markdown {
                top_line: obj.get("top_line")?.as_u64()? as usize,
            }),
            "epub" => Some(ReadingProgress::Epub {
                top_line: obj.get("top_line")?.as_u64()? as usize,
            }),
            "pdf" => {
                let page_index = obj.get("page_index")?.as_u64()? as usize;
                let in_page_frac = obj.get("in_page_frac")?.as_f64()? as f32;
                let zoom = obj.get("zoom")?.as_f64()? as f32;
                if !in_page_frac.is_finite()
                    || !zoom.is_finite()
                    || !(0.0..=1.0).contains(&in_page_frac)
                    || zoom <= 0.0
                {
                    return None;
                }
                Some(ReadingProgress::Pdf { page_index, in_page_frac, zoom })
            }
            _ => None,
        }
    }
}

/// 规范化路径键：先 `canonicalize`（消 `.`/`..`、接真实盘符大小写形态），失败回退原样。
/// 与 `shell_pdf::bookmarks::normalize_key` 同款姿势（镜像 :33-37）。
pub fn normalize_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// 序列化为整份 JSON 对象（`{ 规范化路径 → ReadingProgress }`；空 map 也落合法空对象）。
pub fn to_json(map: &HashMap<String, ReadingProgress>) -> Value {
    let mut obj = serde_json::Map::new();
    for (key, p) in map {
        obj.insert(key.clone(), p.to_value());
    }
    Value::Object(obj)
}

/// 从 JSON 值解析整份 map（纯函数）：非对象 / 损坏行一律跳过，绝不 panic。
pub fn from_json(v: &Value) -> HashMap<String, ReadingProgress> {
    let mut out = HashMap::new();
    let Some(map) = v.as_object() else {
        return out;
    };
    for (key, item) in map {
        if let Some(p) = ReadingProgress::from_value(item) {
            out.insert(key.clone(), p);
        }
    }
    out
}

/// 从磁盘加载整份阅读进度（`progress_path` 缺失 / JSON 损坏 → 空 map）。
pub fn load(progress_path: &Path) -> HashMap<String, ReadingProgress> {
    let Ok(text) = std::fs::read_to_string(progress_path) else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    from_json(&v)
}

/// A4 进程级缓存（`OnceLock<Mutex<Option<(PathBuf, map)>>>`）：条目带**路径守卫**——
/// 只有缓存路径与本次请求一致才复用（防测试/外部换路径读到陈旧盘面）；`save` 成功
/// 同步回写缓存 → 进程内后续读取拿到最新盘面（本应用单进程、无外部改写，安全）。
static PROGRESS_CACHE: OnceLock<StdMutex<Option<(PathBuf, HashMap<String, ReadingProgress>)>>> =
    OnceLock::new();

fn progress_cache() -> &'static StdMutex<Option<(PathBuf, HashMap<String, ReadingProgress>)>> {
    PROGRESS_CACHE.get_or_init(|| StdMutex::new(None))
}

/// 取整份阅读进度（进程级缓存一次读盘；`save` 更新缓存保证进程内一致）。
pub fn load_cached(progress_path: &Path) -> HashMap<String, ReadingProgress> {
    let mut guard = progress_cache().lock().unwrap_or_else(|p| p.into_inner());
    if let Some((p, v)) = guard.as_ref()
        && p.as_path() == progress_path
    {
        return v.clone();
    }
    let v = load(progress_path);
    *guard = Some((progress_path.to_path_buf(), v.clone()));
    v
}

/// 原子落盘整份阅读进度；A4：先写好盘面再回写缓存（缓存镜像盘面，失败不更新）。
pub fn save(progress_path: &Path, map: &HashMap<String, ReadingProgress>) {
    let ok = base::util::atomic_write(progress_path, to_json(map).to_string().as_bytes()).is_ok();
    if ok {
        if let Ok(mut guard) = progress_cache().lock() {
            *guard = Some((progress_path.to_path_buf(), map.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 进程内唯一临时存储路径（每次 save/load 用例一张独立盘面，互不串缓存）。
    fn tmp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gpui-editor-rp-test-{tag}-{nanos}.json"))
    }

    #[test]
    fn reading_progress_json_roundtrip_and_bad_rows_skipped() {
        let mut map = HashMap::new();
        map.insert(
            "D:\\a\\book.md".into(),
            ReadingProgress::Markdown { top_line: 123 },
        );
        map.insert(
            "D:\\a\\book.epub".into(),
            ReadingProgress::Epub { top_line: 45 },
        );
        map.insert(
            "D:\\a\\doc.pdf".into(),
            ReadingProgress::Pdf { page_index: 7, in_page_frac: 0.5, zoom: 1.5 },
        );
        let json = to_json(&map);
        assert_eq!(from_json(&json), map, "往返应一致");

        // 损坏行（缺字段 / 非数字 / 比例越界 / NaN / 非对象 / 非数组）一律跳过，不 panic。
        let bad = json!({
            "k1": {"kind": "markdown", "top_line": 3},
            "k2": {"kind": "markdown"},
            "k3": {"kind": "pdf", "page_index": 2, "in_page_frac": 1.2, "zoom": 1.0},
            "k4": {"kind": "pdf", "page_index": 2, "in_page_frac": 0.5, "zoom": 0.0},
            "k5": {"kind": "pdf", "page_index": 2, "in_page_frac": null, "zoom": 1.0},
            "k6": {"kind": "pdf", "page_index": 2, "in_page_frac": 0.5, "zoom": -1.0},
            "k7": "字符串",
            "k8": [1, 2, 3],
            "k9": {"kind": "unknown"},
        });
        let parsed = from_json(&bad);
        assert_eq!(
            parsed["k1"],
            ReadingProgress::Markdown { top_line: 3 },
            "合法行应保留"
        );
        assert_eq!(parsed.len(), 1, "其余损坏行应全部跳过");

        // 空 / 非对象 → 空 map。
        assert!(from_json(&Value::Null).is_empty());
        assert!(from_json(&json!([])).is_empty());
    }

    #[test]
    fn normalize_key_canonicalizes_absolute_paths() {
        if let Ok(canon) = std::fs::canonicalize(".") {
            let key = normalize_key(Path::new("."));
            assert_eq!(key, canon.to_string_lossy(), "`.` 应被解析成绝对键");
        }
        // 不存在路径 fallback 原样。
        let missing = PathBuf::from("Z:\\nonexistent\\missing\\.txt");
        assert_eq!(normalize_key(&missing), missing.to_string_lossy().into_owned());
    }

    #[test]
    fn save_load_roundtrip_on_disk() {
        let path = tmp_path("disk");
        let mut map = HashMap::new();
        map.insert(
            "C:\\x.md".into(),
            ReadingProgress::Markdown { top_line: 88 },
        );
        save(&path, &map);
        assert_eq!(load(&path), map, "落盘后应原样读回");

        // 清空盘面 → 空 map。
        save(&path, &HashMap::new());
        assert!(load(&path).is_empty(), "空 map 落盘读回应为空");

        // 缺失 / 损坏 JSON → 空 map。
        let missing = tmp_path("missing");
        assert!(load(&missing).is_empty());
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(load(&path).is_empty(), "损坏 JSON 应安全回退空 map");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&missing);
    }

    #[test]
    fn load_cached_misses_on_path_change() {
        let path_a = tmp_path("cache-a");
        let path_b = tmp_path("cache-b");
        save(&path_a, &HashMap::from([("K".into(), ReadingProgress::Markdown { top_line: 1 })]));
        save(&path_b, &HashMap::from([("K".into(), ReadingProgress::Markdown { top_line: 2 })]));
        // 首次读 A 缓存；换路径 B 不应复用 A 的盘面。
        assert_eq!(
            load_cached(&path_a)["K"],
            ReadingProgress::Markdown { top_line: 1 }
        );
        assert_eq!(
            load_cached(&path_b)["K"],
            ReadingProgress::Markdown { top_line: 2 },
            "缓存条目带路径守卫：换路径须重读盘面"
        );
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
