//! PDF 页面的**磁盘二级缓存**：内存页位图预算淘汰后，把页以**紧凑 BGRA**（去 stride）
//! 落盘为每页一个文件；再次翻回时**直接读盘、`Frame::from_raw` 重建** `RenderImage`
//! （毫秒级），无需重新栅格化。磁盘页数用 LRU 封顶，避免原始 BGRA 撑爆临时目录。
//!
//! - 写入由调用方放到**后台 executor**（异步）；读回在主线程（NVMe ~1ms，可接受）。
//! - 状态（关闭标记 + LRU 顺序）放在 [`CacheInner`] 里由异步写与主线程读共享。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::pdf::CACHE_DISK_PAGES;

/// 磁盘缓存文件头魔数（4 字节）+ 宽高各 4 字节 + 紧凑 BGRA 像素。
const CACHE_MAGIC: &[u8; 4] = b"GPBG";
const CACHE_HEADER: usize = 12;

/// 磁盘二级页缓存的共享状态（关闭标记 + LRU 顺序）。
#[derive(Debug)]
pub(crate) struct CacheInner {
    pub(crate) dir: PathBuf,
    /// 已关闭标记：`clear()` 后置位，阻止迟到的后台落盘任务重建目录。
    pub(crate) closed: AtomicBool,
    /// 目前在磁盘上的页号（最近使用在队尾）。
    pub(crate) lru: std::sync::Mutex<Vec<usize>>,
    /// 磁盘页数上限。LRU 淘汰到该上限。
    pub(crate) budget: usize,
}

/// 磁盘二级页缓存句柄（可克隆共享）。
#[derive(Clone, Debug)]
pub(crate) struct PageCache {
    pub(crate) inner: Arc<CacheInner>,
}

impl PageCache {
    /// 以 `pdf_path` 的文件名在系统临时目录开一个分片缓存目录。
    pub(crate) fn new(pdf_path: &Path) -> Self {
        let dir = std::env::temp_dir()
            .join("gpui-editor-pdf-cache")
            .join(fingerprint(pdf_path));
        // 沿用旧格式目录（可能残留 `.bin`/`.png` 旧缓存文件）不主动清理；读盘按魔数跳过损坏文件。
        Self {
            inner: Arc::new(CacheInner {
                dir,
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget: CACHE_DISK_PAGES,
            }),
        }
    }

    /// 读取共享内部状态（供异步落盘任务持有一份克隆）。
    pub(crate) fn inner_arc(&self) -> Arc<CacheInner> {
        self.inner.clone()
    }

    /// 从磁盘读取某页 BGRA（缺失/损坏返回 None）。供主线程快速恢复：
    /// 命中后直接 `render_image_from_bgra`，毫秒级、无冻结。
    pub(crate) fn load_bgra(&self, idx: usize) -> Option<(Vec<u8>, u32, u32)> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return None;
        }
        let path = self.inner.dir.join(format!("{idx:05}.bin"));
        let data = std::fs::read(path).ok()?;
        if data.len() < CACHE_HEADER || &data[..4] != CACHE_MAGIC {
            return None;
        }
        let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let expect = CACHE_HEADER + (w as usize) * (h as usize) * 4;
        if data.len() != expect {
            return None;
        }
        let bgra = data[CACHE_HEADER..].to_vec();
        // 标记为最近使用
        if let Ok(mut lru) = self.inner.lru.lock() {
            lru.retain(|&p| p != idx);
            lru.push(idx);
            let excess = lru.len().saturating_sub(self.inner.budget);
            if excess > 0 {
                lru.drain(0..excess);
            }
        }
        Some((bgra, w, h))
    }

    /// 清理本 PDF 的整个缓存目录（关闭标签页时调用）。
    pub(crate) fn clear(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&self.inner.dir);
    }
}

/// 后台落盘：把一页紧凑 BGRA 写成 `idx.bin`（带魔数与宽高头），并按 LRU 封顶磁盘页数。
/// 在后台 executor 执行，不触碰主线程 / pdfium 工作线程。
pub(crate) fn save_bgra(
    inner: Arc<CacheInner>,
    idx: usize,
    bgra: Vec<u8>,
    w: u32,
    h: u32,
) {
    if inner.closed.load(Ordering::Relaxed) {
        return;
    }
    let _ = std::fs::create_dir_all(&inner.dir);
    let mut data = Vec::with_capacity(CACHE_HEADER + bgra.len());
    data.extend_from_slice(CACHE_MAGIC);
    data.extend_from_slice(&w.to_le_bytes());
    data.extend_from_slice(&h.to_le_bytes());
    data.extend_from_slice(&bgra);
    let _ = std::fs::write(inner.dir.join(format!("{idx:05}.bin")), data);
    // 超出预算：淘汰最久未用的页（异步删除，失败静默）
    let evict: Vec<usize> = {
        let mut lru = inner.lru.lock().unwrap_or_else(|e| e.into_inner());
        lru.retain(|&p| p != idx);
        lru.push(idx);
        if lru.len() > inner.budget {
            let extra = lru.len() - inner.budget;
            lru.drain(0..extra).collect()
        } else {
            Vec::new()
        }
    };
    for page in evict {
        let _ = std::fs::remove_file(inner.dir.join(format!("{page:05}.bin")));
    }
}

/// 用文件名做缓存目录指纹（防路径含非法字符）。
fn fingerprint(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pdf".into());
    // 简单散列：文件大小 + 名称字节异或，避免依赖额外 crate
    let mut hash = 0x9e37u64.wrapping_mul(name.len() as u64);
    for b in name.bytes() {
        hash = hash.wrapping_mul(131).wrapping_add(b as u64);
    }
    format!("{:016x}-{}", hash, sanitize(&name))
}

/// 只保留安全字符，避免路径分隔符/通配等被误用。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache(dir: std::path::PathBuf, budget: usize) -> PageCache {
        PageCache {
            inner: Arc::new(CacheInner {
                dir,
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget,
            }),
        }
    }

    #[test]
    fn page_cache_bgra_roundtrip() {
        // 临时目录里写一页 BGRA、再读回，验证文件头 + 像素完整
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = make_cache(dir.clone(), CACHE_DISK_PAGES);
        for idx in [0usize, 7usize] {
            let w = 16 + idx as u32;
            let h = 32u32;
            let mut bgra = Vec::new();
            for v in 0..(w * h) as u32 {
                bgra.extend_from_slice(&[(v & 0xff) as u8, ((v >> 8) & 0xff) as u8, 0x7f, 0xff]);
            }
            save_bgra(cache.inner.clone(), idx, bgra, w, h);
        }
        let (back, pw, ph) = cache.load_bgra(0).expect("第 0 页应读回");
        assert_eq!((pw, ph), (16, 32), "宽高应一致");
        assert_eq!(back.len(), 16 * 32 * 4, "像素字节数应一致");
        let (back7, pw7, ph7) = cache.load_bgra(7).expect("第 7 页应读回");
        assert_eq!((pw7, ph7), (23, 32));
        assert_eq!(back7[0..4], [0, 0, 0x7f, 0xff], "首像素 BGRA 应一致");
        // 不存在的页 → None
        assert!(cache.load_bgra(99).is_none(), "缺失页应返回 None");
        // 关闭后拒读（模拟已释放）
        cache.clear();
        assert!(cache.load_bgra(0).is_none(), "clear 后不应再读");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_ignores_corrupt_files() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-corrupt",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00000.bin"), b"not-a-pdf-cache").unwrap();
        std::fs::write(dir.join("00001.bin"), b"GPBG").unwrap();
        let cache = make_cache(dir.clone(), CACHE_DISK_PAGES);
        assert!(cache.load_bgra(0).is_none(), "魔数不对应忽略");
        assert!(cache.load_bgra(1).is_none(), "头不全应忽略");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_respects_disk_budget() {
        // 预算 = 小数字：写入超过预算的页后，最久未用的页文件应被删掉（LRU 淘汰）
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-lru",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = make_cache(dir.clone(), 4);
        for idx in 0..8usize {
            let bgra = vec![0x11u8, 0x22, 0x33, 0xff].repeat(64); // 8x8 像素
            save_bgra(cache.inner.clone(), idx, bgra, 8, 8);
        }
        // 写入 0..8 → 预算 4，最旧的 0..3 应被淘汰，4..7 仍在
        for idx in 0..4usize {
            assert!(
                cache.load_bgra(idx).is_none(),
                "预算内最旧的页 {idx} 应被 LRU 淘汰"
            );
        }
        for idx in 4..8usize {
            assert!(
                cache.load_bgra(idx).is_some(),
                "最近的页 {idx} 应保留在磁盘"
            );
        }
        // 读 4 会把它标记为最近使用 → 下一次写 8 时淘汰的是 5
        cache.load_bgra(4);
        let bgra = vec![0xffu8; 8 * 8 * 4];
        save_bgra(cache.inner.clone(), 8, bgra, 8, 8);
        assert!(cache.load_bgra(4).is_some(), "最近用过的页 4 不应被淘汰");
        assert!(cache.load_bgra(5).is_none(), "最久未用的页 5 应被淘汰");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
