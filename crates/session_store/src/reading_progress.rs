//! 「阅读进度恢复」持久化存储层：`{ 规范化文件路径 → ReadingProgress }`，独立 JSON 文件
//! （文件名由调用方注入：默认 `%APPDATA%\vuick\reading_progress.json`，测试走隔离
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
use std::path::Path;
use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::path_key_json_store::{self, Cache, PathKeyJsonStore};

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
            ReadingProgress::Pdf {
                page_index,
                in_page_frac,
                zoom,
            } => json!({
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
                Some(ReadingProgress::Pdf {
                    page_index,
                    in_page_frac,
                    zoom,
                })
            }
            _ => None,
        }
    }
}

/// 规范化路径键：统一收口于 [`path_key_json_store::normalize_key`]（canonicalize + 失败回退
/// 原样），与 `shell_pdf::bookmarks::normalize_key` 规则逐字符一致。
pub fn normalize_key(path: &Path) -> String {
    path_key_json_store::normalize_key(path)
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

/// A4 进程级缓存（路径守卫见 [`path_key_json_store::Cache`]）：条目带路径守卫——只有缓存
/// 路径与本次请求一致才复用（防测试/外部换路径读到陈旧盘面）；`save` 成功同步回写缓存。
static PROGRESS_CACHE: Cache<HashMap<String, ReadingProgress>> = OnceLock::new();

/// 绑定本次存根（路径 + 本模块缓存 + 编解码），读写、缓存全部走共享层（语义与旧实现逐字
/// 一致：缺失/损坏 → 空、写败静默、成功回写缓存）。
fn store(progress_path: &Path) -> PathKeyJsonStore<HashMap<String, ReadingProgress>> {
    PathKeyJsonStore::new(
        progress_path.to_path_buf(),
        &PROGRESS_CACHE,
        from_json,
        to_json,
    )
}

/// 剪掉「目标文件已不存在」的过期路径键（键是规范化/绝对路径、含 `\\?\` 前缀，
/// `std::fs::metadata` 可直接探测），使持久化表不随历史打开路径单调膨胀——此前只有
/// insert、无删除路径，旧文件键只增不减（根因：消费方 `reading_progress` map 无
/// remove/retain/clear）。
/// 代价：每键一次 `fs::metadata` 同步 syscall——只在 flush（防抖 300ms / 关 tab / 退出
/// 兜底）与启动加载时执行，**非逐帧热路径**，量级可接受；map 越大 prune 收益越大。
fn prune_stale_keys(map: &mut HashMap<String, ReadingProgress>) {
    map.retain(|key, _| std::fs::metadata(Path::new(key)).is_ok());
}

/// 从磁盘加载整份阅读进度（`progress_path` 缺失 / JSON 损坏 → 空 map）；**加载即剪枝**：
/// 目标文件已不存在的过期键当场移除（消费方内存 map 同步；落盘面到下一次 `save` 收缩）。
pub fn load(progress_path: &Path) -> HashMap<String, ReadingProgress> {
    let mut map = store(progress_path).load();
    prune_stale_keys(&mut map);
    map
}

/// 取整份阅读进度（进程级缓存一次读盘；`save` 更新缓存保证进程内一致）；同 [`Self::load`]
/// 剪枝过期键。
pub fn load_cached(progress_path: &Path) -> HashMap<String, ReadingProgress> {
    let mut map = store(progress_path).load_cached();
    prune_stale_keys(&mut map);
    map
}

/// 原子落盘整份阅读进度；A4：先写好盘面再回写缓存（缓存镜像盘面，失败不更新）。
/// **落盘前就地剪枝**：目标文件已不存在的过期键先移出 `map` 再写，令表长期有界——调用方
/// 内存 map 被同步修剪，与盘面、缓存三方一致（消费方 actions.rs 经 `&mut self.reading_progress`
/// 传入）。
pub fn save(progress_path: &Path, map: &mut HashMap<String, ReadingProgress>) {
    prune_stale_keys(map);
    store(progress_path).save(map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 进程内唯一临时存储路径（每次 save/load 用例一张独立盘面，互不串缓存）。
    fn tmp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vuick-rp-test-{tag}-{nanos}.json"))
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
            ReadingProgress::Pdf {
                page_index: 7,
                in_page_frac: 0.5,
                zoom: 1.5,
            },
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
        assert_eq!(
            normalize_key(&missing),
            missing.to_string_lossy().into_owned()
        );
    }

    #[test]
    fn save_load_roundtrip_on_disk() {
        let path = tmp_path("disk");
        // 键须指向真实存在的临时文件：`load`/`save` 会剪掉「目标文件不存在」的过期键
        // （防历史路径只增不减），假路径键会被剪成空（见 stale_keys_pruned_on_load）。
        let real = tmp_path("disk-key");
        std::fs::write(&real, "x").unwrap();
        let key = normalize_key(&real);
        let mut map = HashMap::new();
        map.insert(key, ReadingProgress::Markdown { top_line: 88 });
        save(&path, &mut map);
        assert_eq!(load(&path), map, "落盘后应原样读回");

        // 清空盘面 → 空 map。
        save(&path, &mut HashMap::new());
        assert!(load(&path).is_empty(), "空 map 落盘读回应为空");

        // 缺失 / 损坏 JSON → 空 map。
        let missing = tmp_path("missing");
        assert!(load(&missing).is_empty());
        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(load(&path).is_empty(), "损坏 JSON 应安全回退空 map");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&missing);
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn load_cached_misses_on_path_change() {
        let path_a = tmp_path("cache-a");
        let path_b = tmp_path("cache-b");
        // 键须是真实存在的临时文件（假键会被剪枝成空 map，缓存用例读不到值）。
        let fa = tmp_path("cache-key-a");
        let fb = tmp_path("cache-key-b");
        std::fs::write(&fa, "a").unwrap();
        std::fs::write(&fb, "b").unwrap();
        let ka = normalize_key(&fa);
        let kb = normalize_key(&fb);
        let mut ma = HashMap::from([(ka.clone(), ReadingProgress::Markdown { top_line: 1 })]);
        let mut mb = HashMap::from([(kb.clone(), ReadingProgress::Markdown { top_line: 2 })]);
        save(&path_a, &mut ma);
        save(&path_b, &mut mb);
        // 首次读 A 缓存；换路径 B 不应复用 A 的盘面。
        assert_eq!(
            load_cached(&path_a)[&ka],
            ReadingProgress::Markdown { top_line: 1 }
        );
        assert_eq!(
            load_cached(&path_b)[&kb],
            ReadingProgress::Markdown { top_line: 2 },
            "缓存条目带路径守卫：换路径须重读盘面"
        );
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let _ = std::fs::remove_file(&fa);
        let _ = std::fs::remove_file(&fb);
    }

    /// 守门（红→绿）：已删除文件的进度键在下一次加载 / 落盘后被移除，表不再随历史路径
    /// 单调膨胀。撤销 `prune_stale_keys` 的调用 → `load`/`save` 仍保留全部键 = 红。
    #[test]
    fn stale_keys_pruned_on_load() {
        let path = tmp_path("prune-load");
        let alive = tmp_path("prune-alive");
        std::fs::write(&alive, "x").unwrap();
        let doomed = tmp_path("prune-doomed");
        std::fs::write(&doomed, "x").unwrap();

        let alive_key = normalize_key(&alive);
        let doomed_key = normalize_key(&doomed);
        let ghost = "Z:\\ghost\\never-existed.md";

        let mut map = HashMap::new();
        map.insert(alive_key.clone(), ReadingProgress::Markdown { top_line: 1 });
        map.insert(
            doomed_key.clone(),
            ReadingProgress::Markdown { top_line: 2 },
        );
        map.insert(ghost.into(), ReadingProgress::Markdown { top_line: 3 });
        save(&path, &mut map);

        // 落盘后删除 doomed 文件 → 重载应只剩 alive。
        std::fs::remove_file(&doomed).unwrap();
        let loaded = load(&path);
        assert!(loaded.contains_key(&alive_key), "仍存在的文件键应保留");
        assert!(
            !loaded.contains_key(&doomed_key),
            "已删文件的键应在加载时移除"
        );
        assert!(
            !loaded.contains_key(ghost),
            "从未存在的路径键应在加载时移除"
        );
        assert_eq!(loaded.len(), 1, "表不随历史键膨胀：只剩存在文件");

        // 盘面同步收缩：再落盘一次后盘上也不含已删键。
        let mut cur = load_cached(&path);
        save(&path, &mut cur);
        let on_disk = load(&path);
        assert_eq!(on_disk.len(), 1, "落盘面与内存一致（无已删键）");
        assert!(on_disk.contains_key(&alive_key));

        let _ = std::fs::remove_file(&alive);
        let _ = std::fs::remove_file(&path);
    }

    /// 守门（红→绿）：`save` 就地剪枝过期键并令表有界（1 个真实文件键 + 100 个不存在键
    /// → 落盘后仅剩 1）。撤销 `prune_stale_keys` 调用 → `map.len()==101` = 红。
    #[test]
    fn save_prunes_stale_keys_and_bounds_table() {
        let path = tmp_path("prune-save");
        let alive = tmp_path("prune-save-alive");
        std::fs::write(&alive, "x").unwrap();
        let alive_key = normalize_key(&alive);

        let mut map = HashMap::new();
        map.insert(alive_key.clone(), ReadingProgress::Markdown { top_line: 5 });
        for i in 0..100 {
            map.insert(
                format!("Z:\\ghost-{i}.md"),
                ReadingProgress::Markdown { top_line: i },
            );
        }
        save(&path, &mut map);
        assert_eq!(
            map.len(),
            1,
            "save 应就地把过期键剪掉（调用方内存 map 同步）"
        );
        let loaded = load(&path);
        assert_eq!(loaded.len(), 1, "落盘表有界：只保留仍存在的文件");
        assert_eq!(
            loaded[&alive_key],
            ReadingProgress::Markdown { top_line: 5 }
        );

        let _ = std::fs::remove_file(&alive);
        let _ = std::fs::remove_file(&path);
    }
}
