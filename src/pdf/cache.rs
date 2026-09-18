//! PDF 页面的**磁盘二级缓存**：内存页位图预算淘汰后，把页以 **WebP-Lossless 压缩**
//! 落盘为每页一个文件；再次翻回时**直接读盘、解码、`Frame::from_raw` 重建**
//! `RenderImage`（毫秒级），无需重新栅格化。磁盘预算用 LRU 封顶，避免撑爆临时目录。
//!
//! - 磁盘格式：`魔数(4) + u32 宽 + u32 高 + WebP(VP8L) 体`。WebP lossless 对 PDF
//!   文字页压缩比高（~10:1），BGRA 缓冲以 Rgba8 容器无损往返、字节逐像素保真。
//! - 写入由调用方放到**后台 executor**（异步，编码不在 pdfium worker、不在主线程）；
//!   读回在主线程（压缩后 ~几百 KB，解码 ~ms 级，可接受）。
//! - 状态（关闭标记 + LRU 顺序）放在 [`CacheInner`] 里由异步写与主线程读共享。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::pdf::{CACHE_DISK_BYTES, CACHE_THUMB_BYTES};

/// 磁盘缓存文件头魔数（4 字节）+ 宽高各 4 字节 + WebP 体。
const CACHE_MAGIC: &[u8; 4] = b"GPW1";
const CACHE_HEADER: usize = 12;

/// 磁盘二级页缓存的共享状态（关闭标记 + 全尺寸/缩略图两套 LRU）。
#[derive(Debug)]
pub(crate) struct CacheInner {
    pub(crate) dir: PathBuf,
    /// 已关闭标记：`clear()` 后置位，阻止迟到的后台落盘任务重建目录。
    pub(crate) closed: AtomicBool,
    /// 目前在磁盘上的全尺寸页号 + 各自文件字节数（最近使用在队尾）。
    pub(crate) lru: std::sync::Mutex<Vec<(usize, u64)>>,
    /// 磁盘字节上限（全尺寸）。按累计字节数 LRU 淘汰到该上限（最多保留 1 页兜底）。
    pub(crate) budget: u64,
    /// 目前在磁盘上的缩略图页号 + 字节数。
    pub(crate) lru_thumbs: std::sync::Mutex<Vec<(usize, u64)>>,
    /// 磁盘字节上限（缩略图，`CACHE_THUMB_BYTES`）。缩略图独立预算，
    /// 全尺寸被挤掉后仍常驻盘——回翻时先出低清占位再补全尺寸。
    pub(crate) thumb_budget: u64,
    /// 缓存版本（缩放/质量变化时递增）：文件名带版本前缀，旧版本位图像素密度不符，
    /// 读盘键失效 → 按新密度重新栅格化；旧文件在递增时清理。
    pub(crate) version: std::sync::atomic::AtomicU32,
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
                budget: CACHE_DISK_BYTES,
                lru_thumbs: std::sync::Mutex::new(Vec::new()),
                thumb_budget: CACHE_THUMB_BYTES,
                version: std::sync::atomic::AtomicU32::new(0),
            }),
        }
    }

    /// 读取共享内部状态（供异步落盘任务持有一份克隆）。
    pub(crate) fn inner_arc(&self) -> Arc<CacheInner> {
        self.inner.clone()
    }

    /// 缩放/质量变化：递增缓存版本并清理旧版本文件与 LRU。
    /// 旧版本的位图像素密度与当前显示倍率不符，读盘键（文件）随之失效 → 重新栅格化。
    pub(crate) fn bump_version(&self) {
        let v = self
            .inner
            .version
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if let Ok(mut lru) = self.inner.lru.lock() {
            lru.clear();
        }
        if let Ok(mut lru) = self.inner.lru_thumbs.lock() {
            lru.clear();
        }
        let prefix = format!("v{v:03}-");
        if let Ok(entries) = std::fs::read_dir(&self.inner.dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('v') && name.ends_with(".bin") && !name.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// 版本化页文件名：`v<version>-<idx>.bin`。
    fn file_path(dir: &std::path::Path, version: u32, idx: usize) -> std::path::PathBuf {
        dir.join(format!("v{version:03}-{idx:05}.bin"))
    }

    /// 版本化缩略图文件名：`v<version>-<idx>t.bin`（与全尺寸同版本键，区分后缀）。
    fn thumb_path(dir: &std::path::Path, version: u32, idx: usize) -> std::path::PathBuf {
        dir.join(format!("v{version:03}-{idx:05}t.bin"))
    }

    /// 从磁盘读取某页 BGRA（缺失/损坏返回 None）。供主线程快速恢复：
    /// 命中后直接 `render_image_from_bgra`，毫秒级、无冻结。
    pub(crate) fn load_bgra(&self, idx: usize) -> Option<(Vec<u8>, u32, u32)> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return None;
        }
        load_bgra_inner(&*self.inner, idx)
    }

    /// 磁盘上是否**可能**存在某页（无 IO，仅查 LRU 记账；`bump_version` 会清空 LRU，
    /// 因此 LRU 成员即当前版本文件）。供主线程便宜地预判「走异步读盘 vs 直接栅格化」，
    /// 避免对肯定缺失的文件做磁盘 syscall。
    pub(crate) fn contains(&self, idx: usize) -> bool {
        if self.inner.closed.load(Ordering::Relaxed) {
            return false;
        }
        self.inner
            .lru
            .lock()
            .map(|l| l.iter().any(|&(p, _)| p == idx))
            .unwrap_or(false)
    }

    /// 从磁盘读取某页 L0 缩略图 BGRA（缺失/损坏返回 None）。文件极小（几十 KB），
    /// 主线程同步读 ~1ms，可接受；由 view 层在拖动/回翻时先铺低清占位。
    pub(crate) fn load_thumb(&self, idx: usize) -> Option<(Vec<u8>, u32, u32)> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return None;
        }
        let version = self.inner.version.load(Ordering::Relaxed);
        let path = Self::thumb_path(&self.inner.dir, version, idx);
        let data = std::fs::read(path).ok()?;
        if data.len() < CACHE_HEADER || &data[..4] != CACHE_MAGIC {
            return None;
        }
        let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let (bgra, dw, dh) = decode_webp(&data[CACHE_HEADER..])?;
        if dw != w || dh != h {
            return None;
        }
        if let Ok(mut lru) = self.inner.lru_thumbs.lock() {
            for page in lru_touch(&mut lru, self.inner.thumb_budget, idx, data.len() as u64) {
                let _ = std::fs::remove_file(Self::thumb_path(&self.inner.dir, version, page));
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

/// 后台落盘：把一页紧凑 BGRA 编码为 **WebP-Lossless** 后写成 `idx.bin`
/// （带魔数与宽高头），并按 LRU 封顶磁盘页数。
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
    let Some(payload) = encode_webp(&bgra, w, h) else {
        return; // 编码失败：跳过落盘（读侧按缺失处理，不污染缓存）
    };
    let _ = std::fs::create_dir_all(&inner.dir);
    let version = inner.version.load(Ordering::Relaxed);
    let mut data = Vec::with_capacity(CACHE_HEADER + payload.len());
    data.extend_from_slice(CACHE_MAGIC);
    data.extend_from_slice(&w.to_le_bytes());
    data.extend_from_slice(&h.to_le_bytes());
    data.extend_from_slice(&payload);
    let file_len = data.len() as u64;
    let _ = std::fs::write(PageCache::file_path(&inner.dir, version, idx), data);
    // 登记最近使用，并按字节预算淘汰最久未用的页（异步删除，失败静默）
    let evict: Vec<usize> = {
        let mut lru = inner.lru.lock().unwrap_or_else(|e| e.into_inner());
        lru_touch(&mut lru, inner.budget, idx, file_len)
    };
    for page in evict {
        let _ = std::fs::remove_file(PageCache::file_path(&inner.dir, version, page));
    }
}

/// LRU 记账：把 `(页号, 文件字节数)` 标记为最近使用（队尾），累计超预算时
/// **从最久未用开始**淘汰，直到回到预算内；**保底保留最近的 1 页**
/// （单页超预算也允许存在，防 A3/高缩放大页被自己的预算挤掉）。
/// 返回需要删除文件的页号。
fn lru_touch(lru: &mut Vec<(usize, u64)>, budget: u64, idx: usize, data_len: u64) -> Vec<usize> {
    lru.retain(|&(p, _)| p != idx);
    lru.push((idx, data_len));
    let mut evict: Vec<usize> = Vec::new();
    let mut total: u64 = lru.iter().map(|&(_, b)| b).sum();
    while total > budget && lru.len() > 1 {
        let (old, bytes) = lru.remove(0);
        evict.push(old);
        total = total.saturating_sub(bytes);
    }
    evict
}

/// 后台落盘：把一页 L0 缩略图 BGRA 编码为 WebP 后写成 `idx t.bin`（独立缩略图 LRU 预算）。
/// 在后台 executor 执行，不触碰主线程 / pdfium 工作线程。
pub(crate) fn save_bgra_thumb(
    inner: Arc<CacheInner>,
    idx: usize,
    bgra: Vec<u8>,
    w: u32,
    h: u32,
) {
    if inner.closed.load(Ordering::Relaxed) {
        return;
    }
    let Some(payload) = encode_webp(&bgra, w, h) else {
        return;
    };
    let _ = std::fs::create_dir_all(&inner.dir);
    let version = inner.version.load(Ordering::Relaxed);
    let mut data = Vec::with_capacity(CACHE_HEADER + payload.len());
    data.extend_from_slice(CACHE_MAGIC);
    data.extend_from_slice(&w.to_le_bytes());
    data.extend_from_slice(&h.to_le_bytes());
    data.extend_from_slice(&payload);
    let file_len = data.len() as u64;
    let _ = std::fs::write(PageCache::thumb_path(&inner.dir, version, idx), data);
    let evict: Vec<usize> = {
        let mut lru = inner.lru_thumbs.lock().unwrap_or_else(|e| e.into_inner());
        lru_touch(&mut lru, inner.thumb_budget, idx, file_len)
    };
    for page in evict {
        let _ = std::fs::remove_file(PageCache::thumb_path(&inner.dir, version, page));
    }
}

/// 把紧凑 BGRA 缓冲无损编码为 WebP（VP8L）。
///
/// BGRA 在这里只是「Rgba8 字节容器」：编码器按 R,G,B,A 逐通道无损写入，
/// 解码器按同样的通道序还原 → 原始字节逐像素保真（无通道重排、无信息丢失）。
fn encode_webp(bgra: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    use image::codecs::webp::WebPEncoder;
    use image::ExtendedColorType;
    if (w as u64) * (h as u64) * 4 != bgra.len() as u64 {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .encode(bgra, w, h, ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// 把磁盘 WebP 体解码回紧凑 BGRA（字节序与编码前一致，lossless 无损）。
fn decode_webp(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory_with_format(data, image::ImageFormat::WebP).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return None;
    }
    Some((rgba.into_raw(), w, h))
}

/// 在某页文件上做“读盘 + 解码 + LRU 记账”（可后台执行，不触主线程/UI）。
/// 缺失/损坏返回 `None`。读取后按字节预算淘汰最久未用的页文件。
pub(crate) fn load_bgra_inner(inner: &CacheInner, idx: usize) -> Option<(Vec<u8>, u32, u32)> {
    if inner.closed.load(Ordering::Relaxed) {
        return None;
    }
    let version = inner.version.load(Ordering::Relaxed);
    let path = inner.dir.join(format!("v{version:03}-{idx:05}.bin"));
    let data = std::fs::read(path).ok()?;
    if data.len() < CACHE_HEADER || &data[..4] != CACHE_MAGIC {
        return None;
    }
    let w = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let (bgra, dw, dh) = decode_webp(&data[CACHE_HEADER..])?;
    if dw != w || dh != h {
        return None; // 头与解码尺寸不一致 = 文件损坏/版本不符
    }
    let data_len = data.len() as u64;
    if let Ok(mut lru) = inner.lru.lock() {
        for page in lru_touch(&mut lru, inner.budget, idx, data_len) {
            let _ = std::fs::remove_file(inner.dir.join(format!("v{version:03}-{page:05}.bin")));
        }
    }
    Some((bgra, w, h))
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

    fn make_cache(dir: std::path::PathBuf, budget: u64) -> PageCache {
        PageCache {
            inner: Arc::new(CacheInner {
                dir,
                closed: AtomicBool::new(false),
                lru: std::sync::Mutex::new(Vec::new()),
                budget,
                lru_thumbs: std::sync::Mutex::new(Vec::new()),
                thumb_budget: 8 * 1024 * 1024,
                version: std::sync::atomic::AtomicU32::new(0),
            }),
        }
    }

    #[test]
    fn page_cache_thumb_roundtrip() {
        // L0 缩略图独立落盘/读回：文件名带 `t` 后缀，与全尺寸互不干扰
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-thumb",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = make_cache(dir.clone(), 4 * 1024 * 1024);
        let bgra = vec![0x11u8, 0x22, 0x33, 0xff].repeat(16 * 16);
        save_bgra_thumb(cache.inner.clone(), 5, bgra.clone(), 16, 16);
        // 缩略图文件必须是 `...t.bin`（不与全尺寸 `...005.bin` 混淆）
        assert!(
            dir.join("v000-00005t.bin").is_file(),
            "缩略图应写为 v000-00005t.bin"
        );
        assert!(
            !dir.join("v000-00005.bin").exists(),
            "全尺寸文件不应被缩略图写入"
        );
        let (back, pw, ph) = cache.load_thumb(5).expect("缩略图应读回");
        assert_eq!((pw, ph), (16, 16));
        assert_eq!(back.len(), 16 * 16 * 4);
        assert_eq!(back[0..4], [0x11, 0x22, 0x33, 0xff], "首像素应无损还原");
        // 缺失缩略图 → None（与全尺寸缺失一致）
        assert!(cache.load_thumb(99).is_none(), "缺失缩略图应返回 None");
        let _ = std::fs::remove_dir_all(&dir);
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
        let cache = make_cache(dir.clone(), 4 * 1024 * 1024);
        for idx in [0usize, 7usize] {
            let w = 16 + idx as u32;
            let h = 32u32;
            let mut bgra = Vec::new();
            for v in 0..(w * h) as u32 {
                // 稀疏“墨点”文字风：多为 0x11 底，个别像素 0xee 前景——既验证
                // 无损还原（通道不换序），又保证 VP8L 压得下（远小于原始大小）。
                let ink = if v % 17 == 0 { 0xeeu8 } else { 0x11u8 };
                bgra.extend_from_slice(&[ink, 0x22, 0x33, 0xff]);
            }
            save_bgra(cache.inner.clone(), idx, bgra, w, h);
        }
        let (back, pw, ph) = cache.load_bgra(0).expect("第 0 页应读回");
        assert_eq!((pw, ph), (16, 32), "宽高应一致");
        assert_eq!(back.len(), 16 * 32 * 4, "像素字节数应一致");
        // 磁盘上应为 WebP 压缩体：文件远小于原始 16*32*4 字节（回归压缩行为）
        let on_disk = std::fs::metadata(dir.join("v000-00000.bin"))
            .expect("落盘文件应存在")
            .len() as usize;
        assert!(
            on_disk < 16 * 32 * 4,
            "磁盘文件应被压缩：{on_disk}B < 原始 {}B",
            16 * 32 * 4
        );
        let (back7, pw7, ph7) = cache.load_bgra(7).expect("第 7 页应读回");
        assert_eq!((pw7, ph7), (23, 32));
        assert_eq!(back7[0..4], [0xee, 0x22, 0x33, 0xff], "首像素 BGRA 应一致（v=0 为墨点）");
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
        // 版本化文件名（load_bgra 实际查找 `v{ver:03}-{idx:05}.bin`）
        std::fs::write(dir.join("v000-00000.bin"), b"not-a-pdf-cache").unwrap();
        // 魔数正确但 WebP 体是垃圾 → 解码失败应忽略
        std::fs::write(dir.join("v000-00001.bin"), b"GPW1garbage-garbage-garbage").unwrap();
        // 合法 WebP 但头尺寸与解码尺寸不符 → 应忽略
        let webp = encode_webp(&[0u8; 4 * 4], 2, 2).expect("编码应成功");
        let mut div = Vec::new();
        div.extend_from_slice(CACHE_MAGIC);
        div.extend_from_slice(&99u32.to_le_bytes()); // 假装 99x99
        div.extend_from_slice(&99u32.to_le_bytes());
        div.extend_from_slice(&webp);
        std::fs::write(dir.join("v000-00002.bin"), div).unwrap();
        let cache = make_cache(dir.clone(), 4 * 1024 * 1024);
        assert!(cache.load_bgra(0).is_none(), "魔数不对应忽略");
        assert!(cache.load_bgra(1).is_none(), "解码失败应忽略");
        assert!(cache.load_bgra(2).is_none(), "头/解码尺寸不符应忽略");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_respects_disk_budget() {
        // 字节预算 = 恰好 1 页压缩体大小：验证「按累计字节淘汰最久未用 + 保底保留 1 页」
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-lru",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bgra = vec![0x11u8, 0x22, 0x33, 0xff].repeat(64); // 8x8 像素
        // 先落一页量出压缩体大小，再用它做预算（确定性，不依赖具体压缩率）
        let probe = make_cache(dir.clone(), 0);
        save_bgra(probe.inner.clone(), 3, bgra.clone(), 8, 8);
        let one_page = std::fs::metadata(dir.join("v000-00003.bin")).unwrap().len();
        drop(probe);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = make_cache(dir.clone(), one_page);

        save_bgra(cache.inner.clone(), 3, bgra.clone(), 8, 8);
        assert!(cache.load_bgra(3).is_some(), "预算内第 3 页应可读回");
        // 写 4 → 累计超预算，最旧的 3 被淘汰
        save_bgra(cache.inner.clone(), 4, bgra.clone(), 8, 8);
        assert!(cache.load_bgra(3).is_none(), "写 4 后最旧的 3 应被淘汰");
        assert!(cache.load_bgra(4).is_some(), "最新的 4 应保留");
        // 写 5 → 4 被淘汰，5 保留
        save_bgra(cache.inner.clone(), 5, bgra.clone(), 8, 8);
        assert!(cache.load_bgra(4).is_none(), "写 5 后 4 应被淘汰");
        assert!(cache.load_bgra(5).is_some(), "最新的 5 应保留");
        // 写 6 → 5 被淘汰，6 保留（始终保底 1 页）
        save_bgra(cache.inner.clone(), 6, bgra.clone(), 8, 8);
        assert!(cache.load_bgra(5).is_none(), "写 6 后 5 应被淘汰");
        assert!(cache.load_bgra(6).is_some(), "最新的 6 应保留");
        // 磁盘累计字节 ≤ 预算（保底 1 页允许恰好恰等于）
        let total: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        assert!(total <= one_page, "磁盘累计字节应 ≤ 预算：{total} ≤ {one_page}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_cache_bump_version_invalidates_old_scale() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-pdf-cache-test-{}-ver",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = make_cache(dir.clone(), 4 * 1024 * 1024);
        // v0 落一页
        let bgra = vec![0x11u8, 0x22, 0x33, 0xff].repeat(64);
        save_bgra(cache.inner.clone(), 3, bgra, 8, 8);
        assert!(cache.load_bgra(3).is_some(), "v0 应能读回");
        // 版本递增（模拟缩放）→ 旧文件失效并清理
        cache.bump_version();
        assert!(cache.load_bgra(3).is_none(), "旧版本位图不应再被读到");
        // 新版本可正常落盘 + 读回
        let bgra2 = vec![0xFFu8; 16 * 16 * 4];
        save_bgra(cache.inner.clone(), 3, bgra2, 16, 16);
        let (back, pw, ph) = cache.load_bgra(3).expect("新版本应读回");
        assert_eq!((pw, ph), (16, 16));
        assert_eq!(back.len(), 16 * 16 * 4);
        // 磁盘上不应残留旧版本文件
        let stale = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("v000-"))
            .count();
        assert_eq!(stale, 0, "旧版本文件应被清理");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
