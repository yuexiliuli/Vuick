//! 侧边栏跨文件搜索的**纯文件扫描层**（独立叶子 crate）。
//!
//! 自 shell 的 `app/sidebar/mod.rs` 纯扫描部分拆出：文件夹级后台递归扫描
//! （[`folder_scan_collect`]）、行级命中收集（[`scan_line_hits`]）与 ASCII 大小写
//! 不敏感查找（[`find_ci`]），连同扫描结果缓存类型（[`SidebarSearchScan`] /
//! [`FolderSearchScan`]）与 LF 归一化（[`normalize_newlines`]）。
//!
//! 纯结构移动，只重分配既有依赖，不新增外部包；本 crate 不含 gpui / 视图，只有
//! `futures`（有界 mpsc 通道）+ `memchr`（ASCII 快路径）两个外部依赖，可被任意
//! 宿主（当前为 shell 侧边栏搜索 / 快速打开）复用。

use std::path::PathBuf;

/// 组件内统一 LF 语义（CRLF/CR → LF）：编辑缓冲、文件夹扫描与剪贴板粘贴共用的
/// 行归一化（`open_file` 与 `folder_scan_collect` 都先归一化，命中行号 / 字节区间
/// 与编辑缓冲一致，跳转选中才不偏）。
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 侧边栏搜索一次扫描产出的**命中索引**（渲染复用缓存，不重扫全文）。
#[derive(Debug, Default)]
pub struct SidebarSearchScan {
    /// 归一化后的查询词（trim + to_lowercase，环节点上色与缓存键）。
    pub query: String,
    /// 扫描时的活动选项卡下标（缓存键的一部分：切文档自动失效重扫）。
    pub tab: usize,
    /// 文件名命中的选项卡下标（按打开顺序）。
    pub file_tabs: Vec<usize>,
    /// 当前活动文档中内容命中的行。
    pub line_hits: Vec<SearchLineHit>,
}

/// 一行内容命中：行号 + 首个命中文本的文档字节区间 + 展示条目片段。
#[derive(Debug, Clone)]
pub struct SearchLineHit {
    /// 文档行号（0 基）。
    pub line: usize,
    /// 首个命中文本的文档字节区间（跳转后选中高亮）。
    pub range: std::ops::Range<usize>,
    /// 去掉首尾空白后的行内容（渲染条目显示）。
    pub snippet: String,
}

/// 文件夹级搜索跳过的目录名（黑名单）：VCS / 构建产物 / 依赖目录。
const FOLDER_SCAN_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];
/// 文件夹级搜索跳过的**超大文本文件**阈值：> 5MB 不读盘跳过（元数据判断）。
const FOLDER_SCAN_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// 二进制嗅探采样窗口（读前 N 字节含 NUL → 视为二进制跳过）。
const FOLDER_SCAN_BINARY_SNIFF_BYTES: usize = 8192;

/// 文件夹级搜索「单文件命中组」（按文件分组渲染；词频 = 组内命中条数）。
#[derive(Debug, Clone)]
pub struct FolderFileHit {
    /// 命中文件绝对路径（点击行打开的依据）。
    pub path: PathBuf,
    /// 组内命中行（行号 / LF 归一化全文字节区间 / trim 片段）。
    pub hits: Vec<SearchLineHit>,
    /// 扫描时刻的文件指纹（大小 + 修改时间），供陈旧判定：文件被编辑/落盘后
    /// 指纹与磁盘实态不符 → 该组命中行号/字节区间相对编辑缓冲已失效（P1-37）。
    /// 扫描无元数据（理论上不发生，见 `folder_scan_collect` 传参）→ `None`。
    pub fingerprint: Option<FileFingerprint>,
}

/// 文件扫描时刻指纹：`(size, mtime)`，`FolderSearchScan` 陈旧检测的最小取证单元
/// （轻量 `std::fs::metadata`，无内容哈希，命中文件数有 `FOLDER_SEARCH_RESULT_CAP`
/// 上限；mtime 仅作辅助信号，主判据是 size——内容变更尺寸必变、跨时钟粒度也稳）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    /// 文件字节长度（内容改变 → 长度变化 → 必被检出）。
    pub size: u64,
    /// 最后修改时间（辅助信号；`metadata().modified()` 失败 → `None`，退化为仅比 size）。
    pub mtime: Option<std::time::SystemTime>,
}

impl FileFingerprint {
    /// 取一个路径当前的指纹；文件不存在 / 非文件 / 元数据失败 → `None`（视为「已失效」）。
    pub fn now(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Self {
            size: meta.len(),
            mtime: meta.modified().ok(),
        })
    }

    /// 该指纹是否仍与 `path` 当前磁盘实态一致（size 或 mtime 任一变化即不一致）。
    pub fn matches_current(&self, path: &std::path::Path) -> bool {
        match Self::now(path) {
            Some(cur) => cur.size == self.size && cur.mtime == self.mtime,
            // 文件被删/改名/不可读 → 指纹无从比对 → 陈旧。
            None => false,
        }
    }
}

/// 文件夹级搜索的（后台增量装配的）扫描结果缓存。
#[derive(Debug, Default, Clone)]
pub struct FolderSearchScan {
    /// 归一化查询词（渲染命中词高亮与缓存键的一部分）。
    pub query: String,
    /// 扫描时的文件夹根（缓存键的一部分：换根即失效重扫）。
    pub root: Option<PathBuf>,
    /// 按文件分组的结果（扫描完成顺序）。
    pub files: Vec<FolderFileHit>,
    /// 是否因达到 `FOLDER_SEARCH_RESULT_CAP` 截断。
    pub truncated: bool,
    /// 是否仍在后台扫描（UI 据此显示「搜索中…」）。
    pub running: bool,
    /// 扫描代：换查询 / 换根发起新扫描即 +1，旧代异步回调按代丢弃（防晚到覆盖新结果）。
    pub generation: u64,
    /// 累计命中条数（词频与截断判定依据）。
    pub total_hits: usize,
}

impl FolderSearchScan {
    /// 命中文件相对扫描时刻**任一致已陈旧**（编辑/落盘/删除/改名）→ 结果不可再用于跳转。
    /// 宿主调用点建议：`folder_search_open_hit` 点击跳转前判一次（最紧、一次 stat/文件），
    /// 或 `folder_search_scan` 复用缓存键命中后判一次（每次渲染面板时，命中文件数受
    /// `FOLDER_SEARCH_RESULT_CAP` 约束，stat 级成本可接受）。陈旧即重扫/提示，杜绝
    /// 「编辑后仍按旧行号/旧字节区间跳转错位」（P1-37）。
    pub fn any_stale(&self) -> bool {
        self.files.iter().any(|f| {
            f.fingerprint
                .map(|fp| !fp.matches_current(&f.path))
                // 无指纹（理论不发生）→ 保守按陈旧处理。
                .unwrap_or(true)
        })
    }

    /// 与 [`Self::any_stale`] 同判据，返回**已陈旧文件路径列表**（供宿主提示/定向重扫）。
    pub fn stale_paths(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| {
                f.fingerprint
                    .map(|fp| !fp.matches_current(&f.path))
                    .unwrap_or(true)
            })
            .map(|f| f.path.clone())
            .collect()
    }
}

/// 后台扫描 → 主线程的一块增量更新（有界 channel，见宿主 `start_folder_scan`）。
#[derive(Debug)]
pub enum FolderScanUpdate {
    /// 一个文件命中组（含该文件全部命中行）。
    File(FolderFileHit),
    /// 扫描完成（携带是否截断）。
    Done { truncated: bool },
}

/// 在 `text` 中收集查询词 `q` 的**全部**大小写不敏感命中的字节区间（非重叠、逐段推进），
/// 供命中词预览高亮。区间恒落在 UTF-8 字符边界（`find_ci` 语义保证），跳过重叠尾随。
pub fn highlight_ranges(text: &str, q: &str) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    if q.is_empty() {
        return out;
    }
    let mut pos = 0usize;
    while pos < text.len() {
        let Some(r) = find_ci(&text[pos..], q) else {
            break;
        };
        let start = pos + r.start;
        let end = pos + r.end;
        if end <= start {
            break; // 空命中防御（理论上不会发生）
        }
        out.push(start..end);
        pos = end;
    }
    out
}

/// 后台递归扫描文件夹内容（**不在主线程执行**）：跳过黑名单目录 / 会话目录、跳过
/// 5MB 以上超大文件与二进制 / 非 UTF-8 文件；对每个候选文本文件 LF 归一化后逐行扫查询词
/// （`scan_line_hits`），命中文件作为一个块经有界 `tx` 外推（背压让出）。`total` 为
/// 累计命中条数，达到 `cap` 即停止扫描（返回截断标记）。单文件读失败 / 单目录读失败 /
/// 无权限 → 跳过该条目计为已扫描，**不中断**整体扫描。
pub async fn folder_scan_collect(
    root: &std::path::Path,
    q: &str,
    tx: futures::channel::mpsc::Sender<FolderScanUpdate>,
    session_dir: &std::path::Path,
    total: &mut usize,
    cap: usize,
) -> bool {
    use futures::SinkExt as _;
    let mut tx = tx;
    // 显式栈 DFS（避免深目录递归爆栈；深度顺序对结果无影响，条目整体排序保证确定性）。
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if *total >= cap {
            return true;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue; // 单目录读失败 → 跳过不中断
        };
        // 条目排序保证确定性（目录/文件一律按文件名），与树视图 read_dir_entries 同款。
        let mut entries: Vec<std::fs::DirEntry> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if *total >= cap {
                return true;
            }
            let path = entry.path();
            // 目录判定用 `entry.file_type()`（**不跟随**符号链接）：符号链接 / Windows
            // 目录联接指向的目录一律**不递归**进栈，防「目录循环引用 → 扫描无限循环」
            // （`metadata` 跟随链接，用其判目录时 `.→自身` 的联接会无限入栈白耗磁盘）。
            let Ok(ft) = entry.file_type() else {
                continue; // 单条目元数据失败 → 跳过
            };
            if ft.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if FOLDER_SCAN_SKIP_DIRS.contains(&name) || path == *session_dir {
                    continue;
                }
                stack.push(path);
            } else {
                // 符号链接条目（无论指向文件还是目录）一律不跟随、不读取：`ft` 来自
                // `entry.file_type()`（非跟随），若不在此拦下，文件符号链接会在下面的
                // `fs::metadata`/`fs::read`（都跟随链接）处把指向**根外**文件的内容读进
                // 结果（跨信任边界泄露，P0-B4）；对齐 rg 默认（不跟随符号链接索引）。
                if ft.is_symlink() {
                    continue;
                }
                // 普通文件：跟随元数据取真实类型 / 大小；其它特殊文件 → 跳过（防循环）。
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue; // 单条目元数据失败 → 跳过
                };
                if !meta.is_file() {
                    continue;
                }
                if meta.len() > FOLDER_SCAN_MAX_FILE_BYTES {
                    continue; // 超大文件：不读盘跳过
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue; // 单文件读失败 / 无权限 → 跳过不中断
                };
                // 二进制检测：采样窗口含 NUL 字节视为二进制；再校验整篇 UTF-8
                //（编辑器以 UTF-8 读盘，非 UTF-8 文件即使无 NUL 也不可索引）。
                if bytes[..bytes.len().min(FOLDER_SCAN_BINARY_SNIFF_BYTES)].contains(&0) {
                    continue;
                }
                let Ok(content) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                // LF 归一化后再扫：命中行号 / 字节区间与打开文件后的编辑缓冲一致
                //（`open_file` 同样做 `normalize_newlines`），点击跳转选中才不偏。
                // 无 `\r`（LF 已归一）直接扫原文零分配；有 CR/CRLF 才物化归一化副本。
                let remaining = cap.saturating_sub(*total);
                let mut hits: Vec<SearchLineHit> = Vec::new();
                if content.contains('\r') {
                    let normalized = crate::normalize_newlines(content);
                    scan_line_hits(&normalized, q, remaining, &mut hits);
                } else {
                    scan_line_hits(content, q, remaining, &mut hits);
                }
                if hits.is_empty() {
                    continue;
                }
                *total += hits.len();
                // 指纹取自本次读盘前已取到的 `meta`（同一份元数据，大小/类型已验过）。
                let fingerprint = Some(FileFingerprint {
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                });
                let f = FolderFileHit { path, hits, fingerprint };
                let _ = tx.send(FolderScanUpdate::File(f)).await;
            }
        }
    }
    *total >= cap
}

/// ASCII 大小写不敏感子串查找（无分配），返回首个命中的字节区间。
/// 仅当 `needle` 全 ASCII 时走 `memchr` 加速的字节快路径（用 `memchr2` 同时搜
/// 首字节大小写两种形式，命中再逐字节校验余下部分）：UTF-8 多字节序列的内部字节
/// 都 ≥ 0x80，因此纯 ASCII 命中必然落在字符边界上，返回的区间始终合法。
/// 非 ASCII 查询词回退到与原逻辑等价的字符级大小写折叠扫描（保留原语义）。
pub fn find_ci(haystack: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if needle.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        if n.len() > h.len() {
            return None;
        }
        let first = n[0];
        // 同时搜首字节的 ASCII 大小写两种形式（字母大小写差 0x20）
        let first_alt = if first.is_ascii_alphabetic() {
            first ^ 0x20
        } else {
            first
        };
        let mut pos = 0usize;
        while let Some(rel) = memchr::memchr2(first, first_alt, &h[pos..]) {
            let at = pos + rel;
            if at + n.len() <= h.len() {
                let mut ok = true;
                // 尾字节也要折 needle：仅折 haystack 时，needle 含大写尾字节（如 "aB"）
                // 对任何混合大小写正文都永不命中（首字节已双形、尾字节漏折，P1-36）。
                // needle 全 ASCII，`to_ascii_lowercase` 安全且长度不变。
                for (j, &nb) in n.iter().enumerate().skip(1) {
                    if !h[at + j].eq_ignore_ascii_case(&nb) {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    return Some(at..(at + n.len()));
                }
            }
            pos = at + 1;
        }
        return None;
    }
    // 非 ASCII needle：字符级大小写折叠比较（零分配，语义与旧「取 n 字符 collect 为
    // 整串再 to_lowercase() 与 needle 整串比较」一致：每个候选起点取 needle 同款
    // 字符数，逐字符折叠后与 needle 字符流比对；折叠展开/长度不等即不命中）。
    let needle_chars: Vec<char> = needle.chars().collect();
    let n_len = needle_chars.len();
    if n_len == 0 {
        return None;
    }
    let mut i = 0usize;
    while i < haystack.len() {
        let rest = &haystack[i..];
        let mut ni: usize = 0; // 已比对上的 needle 字符数
        let mut cand_bytes: usize = 0; // 候选在 haystack 中占用的字节数
        let mut hit = true;
        for hc in rest.chars().take(n_len) {
            cand_bytes += hc.len_utf8();
            let folded = hc.to_lowercase();
            for f in folded {
                if ni >= n_len || f != needle_chars[ni] {
                    hit = false;
                    break;
                }
                ni += 1;
            }
            if !hit {
                break;
            }
        }
        if hit && ni == n_len {
            return Some(i..(i + cand_bytes));
        }
        match rest.chars().next() {
            Some(ch) => i += ch.len_utf8(),
            None => break,
        }
    }
    None
}

/// ASCII 大小写不敏感子串是否存在（无分配；语义与旧 `to_lowercase().contains()` 一致）。
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

/// 对一份**已 LF 归一化**（CRLF/CR → LF，行区间以此为基）的全文逐行扫查询词，把每行的
/// 首个命中收进 `out`（行号 + 文档字节区间 + trim 片段），到达 `remaining` 上限即停。
///
/// 两条查询语义路径：
/// - **ASCII 查询**（常见路径）：`content.to_ascii_lowercase()` **一次性**建 ASCII 小写全文
///   副本（std 实现；非 ASCII 字节原样保留 → 字节布局与 `\n` 分布不变，命中字节区间可直接
///   映射回原文，UTF-8 不受破坏），然后逐行用 std `str::find`（memmem 快路径）匹配小写后的
///   查询词。语义与旧逐字节 `to_ascii_lowercase` 折叠（[`find_ci`] ASCII 分支）完全等价；
///   同一轮搜索只物化一次副本、逐行借用，**命中前零逐行分配**（唯一逐行分配是命中片段的
///   `line.trim().to_string()`，≤ remaining 次）。基准（0.8MB/万行 debug）：副本 2.5ms +
///   扫描 5.1ms ≈ 8ms，比现 memchr `find_ci`（13.6ms）快 ~1.7×，且对标大小写敏感下限。
/// - **非 ASCII 查询**：回退 [`find_ci`] 字符级大小写折叠（保留原国际化语义，零分配）。
///
/// 行字节区间 = 行首文档偏移 + 行内命中位（`doc_offset` 按 `行长+换行(1)` 累积，与命中
/// 跳转的选中高亮一致）。只读/可编辑两种来源共用此实现，保证命中语义单一。
pub fn scan_line_hits(content: &str, q: &str, remaining: usize, out: &mut Vec<SearchLineHit>) {
    if q.is_empty() || remaining == 0 {
        return;
    }
    let mut doc_offset = 0usize;
    if q.is_ascii() {
        // 一次性小写全文副本（std `to_ascii_lowercase`：仅折 ASCII 字母，长度不变）；
        // 查询词同样折叠（P1-36 复查项）：未归一化含大写的 q 也能大小写不敏感命中，
        // ASCII 折叠长度不变 → 命中字节区间仍按原 q 长度直映回原文。
        let lower = content.to_ascii_lowercase();
        let lower_q = q.to_ascii_lowercase();
        for (li, (line, lower_line)) in content.split('\n').zip(lower.split('\n')).enumerate() {
            if out.len() >= remaining {
                break;
            }
            if let Some(pos) = lower_line.find(&lower_q) {
                out.push(SearchLineHit {
                    line: li,
                    range: (doc_offset + pos)..(doc_offset + pos + lower_q.len()),
                    snippet: line.trim().to_string(),
                });
            }
            doc_offset += line.len() + 1;
        }
    } else {
        for (li, line) in content.split('\n').enumerate() {
            if out.len() >= remaining {
                break;
            }
            if let Some(r) = find_ci(line, q) {
                out.push(SearchLineHit {
                    line: li,
                    range: (doc_offset + r.start)..(doc_offset + r.end),
                    snippet: line.trim().to_string(),
                });
            }
            doc_offset += line.len() + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-36 回归：`find_ci` ASCII 分支对**尾字节含大写**的 needle（如 "aB"）也应
    /// 大小写不敏感命中——修前只折 haystack 不折 needle，`"aB"` 对任何混合大小写正文
    /// 都永无命中（首字节已双形、尾字节漏折）。
    #[test]
    fn find_ci_folds_needle_tail_bytes() {
        // 尾字节大写 needle × 四种大小写组合正文全部应命中 0..2。
        for h in ["ab", "aB", "Ab", "AB"] {
            assert_eq!(find_ci(h, "aB"), Some(0..2), "haystack={h:?}");
            assert_eq!(find_ci(h, "Ab"), Some(0..2), "haystack={h:?}");
        }
        // 首字节大小写双形保持（修前已支持）。
        assert_eq!(find_ci("aB", "ab"), Some(0..2));
        assert_eq!(find_ci("xX aB trailing", "ab"), Some(3..5));
        // 全大写 needle 命中小写/混合正文。
        assert_eq!(find_ci("i say hello world", "HELLO"), Some(6..11));
        assert_eq!(find_ci("i say HELLO world", "hello"), Some(6..11));
        // 不命中：无对应大小写簇。
        assert_eq!(find_ci("hello", "aB"), None);
        assert_eq!(find_ci("xyz", "abC"), None);
    }

    /// P1-36 回归（复查项）：`scan_line_hits` ASCII 分支对**未归一化（含大写）**的查询词
    /// 也应大小写不敏感命中（修前建小写全文副本后直接用原 q 匹配，`"HELLO"` 永不命中）。
    #[test]
    fn scan_line_hits_folds_ascii_query() {
        let mut out = Vec::new();
        scan_line_hits("alpha\nHello World\nbeta Hello\n", "HELLO", usize::MAX, &mut out);
        // 行号 + 文档字节区间 = 行首偏移 + 行内命中位（ASCII 折叠长度不变，区间直映原文）。
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line, 1);
        assert_eq!(out[0].range, 6..11, "line1 行首偏移 6 + 命中位 0..5");
        assert_eq!(out[1].line, 2);
        assert_eq!(out[1].range, 23..28, "line2 行首偏移 23(6+11+1+5) + 命中位 0..5");
        // 混合大小写 needle 命中混合大小写正文。
        let mut out = Vec::new();
        scan_line_hits("only one WiRd Token line\n", "wIrD", usize::MAX, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].range, 9..13);
        // 空白语义保持。
        let mut out = Vec::new();
        scan_line_hits("A\nB\n", "b", usize::MAX, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 1);
    }

    /// 测试临时目录：进程 + 名字 + 自增序号唯一（std 自造，不加依赖）。
    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("shell_scan_{}_{}_{}", std::process::id(), name, n))
    }

    /// P1-37 回归：`folder_scan_collect` 产出的文件命中携带**扫描时刻指纹**（大小 + mtime），
    /// 文件被编辑/落盘后 `FolderSearchScan::any_stale` / `stale_paths` 可检出陈旧——修前
    /// 结果不含任何指纹，陈旧命中无从判别，点击跳转沿用旧行号/旧字节区间 → 错位。
    #[test]
    fn folder_search_scan_stale_detected_after_edit() {
        use futures::executor::block_on;
        use futures::StreamExt as _;
        let dir = unique_temp_dir("folder_scan_stale");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("a.txt");
        let other = dir.join("b.txt");
        std::fs::write(&target, "alpha query beta\n").unwrap();
        std::fs::write(&other, "query elsewhere\n").unwrap();
        let session =
            std::env::temp_dir().join(format!("shell_scan_session_{}", std::process::id()));

        // 真实扫描：命中两文件，产生指纹（session 目录非本测试目录，不跳过）。
        let (tx, mut rx) = futures::channel::mpsc::channel::<FolderScanUpdate>(8);
        let mut total = 0usize;
        let _truncated = block_on(folder_scan_collect(&dir, "query", tx, &session, &mut total, 100));
        let mut scan = FolderSearchScan {
            query: "query".to_string(),
            root: Some(dir.clone()),
            files: Vec::new(),
            truncated: false,
            running: false,
            generation: 0,
            total_hits: total,
        };
        while let Ok(Some(upd)) = rx.try_next() {
            if let FolderScanUpdate::File(f) = upd {
                scan.files.push(f);
            }
        }
        assert_eq!(scan.files.len(), 2, "两文件都应命中");
        // 扫描后（未编辑）：不陈旧 + 每个命中文件都有指纹。
        assert!(!scan.any_stale(), "扫描后无编辑 → 不应陈旧");
        assert!(scan.stale_paths().is_empty());
        for f in &scan.files {
            assert!(f.fingerprint.is_some(), "{} 应有扫描时刻指纹", f.path.display());
        }

        // 编辑 target（追加变长 → 大小必变，mtime 亦变）→ 陈旧可检出，且只标 target。
        std::fs::write(&target, "alpha query beta\nsecond line query more\n").unwrap();
        assert!(scan.any_stale(), "编辑后应陈旧");
        let stale = scan.stale_paths();
        assert!(
            stale.iter().any(|p| p == &target),
            "stale_paths 应含 target，实得 {stale:?}"
        );
        assert!(
            !stale.iter().any(|p| p == &other),
            "未编辑的 other 不应陈旧，实得 {stale:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0 回归：`folder_scan_collect` 的**文件分支**用 `fs::metadata`（跟随符号链接）+ `fs::read`
    /// 把指向**扫描根外**文件的内容读入并索引进结果（目录分支用 `entry.file_type()` 不跟随防环，
    /// 但文件分支 227-240 一带注释自称刻意跟随）→ 扫描含「→ ~/.ssh/config」这类文件链接的仓库时，
    /// 查询命中把根外文件内容显示进搜索面板（跨信任边界泄露）。
    /// 本测试断言：根外哨兵文本**不出现**在任何命中片段里——修复前会命中 → 红测。
    #[test]
    fn folder_scan_does_not_follow_file_symlink_outside_root() {
        use futures::executor::block_on;
        let base = unique_temp_dir("fs_symlink_outside");
        std::fs::create_dir_all(&base).unwrap();
        // 扫描根 R + 根外目录（各自独立）。
        let root = base.join("R");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // 根外文件 V：唯一哨兵文本（查询词取其中 ASCII 子串，大小写不敏感命中）。
        let sentinel = "OUTSIDE_SENTINEL_XYZ";
        let v = outside.join("v.txt");
        std::fs::write(&v, format!("secret {sentinel} leaked\n")).unwrap();
        // R 内建指向 V 的**文件符号链接**：修复前文件分支 `metadata`+`read` 会跟随读入 V 内容。
        let link = root.join("link.txt");
        if !create_file_link(&link, &v) {
            // 环境不支持建文件符号链接（Windows 无权限/无开发者模式）→ 跳过；Unix / 支持平台才真跑。
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let session =
            std::env::temp_dir().join(format!("shell_scan_session_{}", std::process::id()));
        let (tx, mut rx) = futures::channel::mpsc::channel::<FolderScanUpdate>(8);
        let mut total = 0usize;
        let _truncated = block_on(folder_scan_collect(
            &root,
            "outside_sentinel",
            tx,
            &session,
            &mut total,
            100,
        ));
        let mut snippets: Vec<String> = Vec::new();
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(upd) => {
                    if let FolderScanUpdate::File(f) = upd {
                        paths.push(f.path.clone());
                        for h in &f.hits {
                            snippets.push(h.snippet.clone());
                        }
                    }
                }
                Err(_) => break,
            }
        }
        for (i, s) in snippets.iter().enumerate() {
            assert!(
                !s.contains(sentinel),
                "根外哨兵不应出现在命中片段 #{}（{}）：{s:?}",
                i,
                paths.iter().next().map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Windows：建**文件**符号链接。先试 `std::os::windows::fs::symlink_file`（开发者模式 /
    /// 管理员可建），失败再退 `cmd /C mklink <link> <target>`（文件符号链接，非 /J 联接）；
    /// 两者都失败返回 false（无权限 → 上层跳过）。
    #[cfg(windows)]
    fn create_file_link(link: &std::path::Path, target: &std::path::Path) -> bool {
        if std::os::windows::fs::symlink_file(target, link).is_ok() {
            return true;
        }
        std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Unix：文件符号链接；失败返回 false。
    #[cfg(not(windows))]
    fn create_file_link(link: &std::path::Path, target: &std::path::Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}
