//! 通用「路径键 + 进程缓存 + 原子写」JSON 持久化存储层（消除 `shell_pdf::bookmarks` 与
//! `session_store::reading_progress` 两处的逐行复制）。
//!
//! - **路径键**：[`normalize_key`]（`canonicalize` + 失败回退原样）统一收口于此（两侧规则
//!   逐字符一致）。
//! - **进程缓存**：`OnceLock<Mutex<Option<(PathBuf, V)>>>`，条目带**路径守卫**——缓存路径与
//!   本次请求一致才复用（reading_progress 多临时路径 / 测试隔离需要；bookmarks 固定单路径下
//!   守卫恒真、与不守卫行为完全一致，故统一为守卫式）。
//! - **原子写**：复用 `base::util::atomic_write`（临时文件 + rename），写败静默回退。
//! - **编解码配置**：整份 map 的**手工 JSON 序列化 / 校验由消费方经函数指针提供**（`decode` /
//!   `encode`）——两侧数据形状与容错规则（坏行跳过 / 数值域校验）不同，无法统一；存储层只做
//!   「读文本 → 解析 → decode / encode → 原子写 → 回写缓存」的骨架。
//!
//! 持久化语义逐字保持：缺失 / 损坏盘面 → 空（`decode(&Value::Null)`，两侧约定非对象 → 空）；
//! `save` 失败静默、成功同步回写进程缓存（缓存镜像盘面）。

use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};

use serde_json::Value;

/// 规范化路径键：先 `canonicalize`（消 `.`/`..`、接真实盘符大小写形态），失败回退原样
/// 字符串。同一文件的后续打开 / 绝对化写法都落到同一键。
pub fn normalize_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// 进程缓存类型：`OnceLock<Mutex<Option<(PathBuf, V)>>>`（路径守卫 + 整份 map）。
pub type Cache<V> = OnceLock<StdMutex<Option<(PathBuf, V)>>>;

/// 路径键 JSON 存储（路径 + 进程缓存 + 编解码配置）。
pub struct PathKeyJsonStore<V: Clone + 'static> {
    path: PathBuf,
    cache: &'static Cache<V>,
    /// 从 JSON 值解析整份 map（容错 / 坏行跳过规则由消费方实现承担）。
    decode: fn(&Value) -> V,
    /// 序列化整份 map 与 JSON（与消费方 `to_json` 语义一致）。
    encode: fn(&V) -> Value,
}

impl<V: Clone + 'static> PathKeyJsonStore<V> {
    pub fn new(
        path: PathBuf,
        cache: &'static Cache<V>,
        decode: fn(&Value) -> V,
        encode: fn(&V) -> Value,
    ) -> Self {
        Self { path, cache, decode, encode }
    }

    fn cache(&self) -> &'static StdMutex<Option<(PathBuf, V)>> {
        self.cache.get_or_init(|| StdMutex::new(None))
    }

    /// 从磁盘加载整份存储（缺失 / JSON 损坏 → 空；绝不 panic）。
    pub fn load(&self) -> V {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return (self.decode)(&Value::Null);
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return (self.decode)(&Value::Null);
        };
        (self.decode)(&v)
    }

    /// 进程级缓存一次读盘（`save` 更新缓存保证进程内一致；缓存路径与本次不符 → 重读盘面）。
    pub fn load_cached(&self) -> V {
        let mut guard = self.cache().lock().unwrap_or_else(|p| p.into_inner());
        if let Some((p, v)) = guard.as_ref()
            && p.as_path() == self.path
        {
            return v.clone();
        }
        let v = self.load();
        *guard = Some((self.path.clone(), v.clone()));
        v
    }

    /// 原子落盘整份存储；先写好盘面再回写缓存（缓存镜像盘面，写败不更新）。
    pub fn save(&self, map: &V) {
        let ok =
            base::util::atomic_write(&self.path, (self.encode)(map).to_string().as_bytes()).is_ok();
        if ok
            && let Ok(mut guard) = self.cache().lock() {
                *guard = Some((self.path.clone(), map.clone()));
            }
    }
}

/// 骨架级回归（锁 load/save/cache 三种动作）：用最小 V（原样 JSON 对象均可）验证共享层
/// 本身不回归——消费侧各自行为由其自有测试兜底。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    type JsonMap = HashMap<String, Value>;

    fn decode(v: &Value) -> JsonMap {
        v.as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn encode(m: &JsonMap) -> Value {
        Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    fn test_cache() -> &'static Cache<JsonMap> {
        static C: Cache<JsonMap> = OnceLock::new();
        &C
    }

    fn tmp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vuick-pjson-test-{tag}-{nanos}.json"))
    }

    fn store(path: &Path) -> PathKeyJsonStore<JsonMap> {
        PathKeyJsonStore::new(path.to_path_buf(), test_cache(), decode, encode)
    }

    #[test]
    fn skeleton_save_load_and_cache() {
        let path = tmp_path("skel");
        let mut map = JsonMap::new();
        map.insert("k".into(), json!({ "n": 1 }));

        store(&path).save(&map);
        assert_eq!(store(&path).load(), map, "落盘后应原样读回");
        assert_eq!(store(&path).load_cached(), map, "缓存命中同盘面");

        // 缺省 / 损坏 JSON → 空 map。
        assert!(store(&tmp_path("skel-missing")).load().is_empty());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(store(&path).load().is_empty(), "损坏 JSON 应安全回退空");

        let _ = std::fs::remove_file(&path);

        // 缓存路径守卫：换路径走新存根不串台。
        let a = tmp_path("skel-a");
        let b = tmp_path("skel-b");
        store(&a)
            .save(&HashMap::from([("K".into(), json!(1))]));
        store(&b)
            .save(&HashMap::from([("K".into(), json!(2))]));
        assert_eq!(store(&a).load_cached()["K"], json!(1));
        assert_eq!(store(&b).load_cached()["K"], json!(2), "换路径须按守卫重读");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}
