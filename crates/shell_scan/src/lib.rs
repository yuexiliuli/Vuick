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
/// >5MB 超大文件与二进制 / 非 UTF-8 文件；对每个候选文本文件 LF 归一化后逐行扫查询词
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
                // 文件（含指向文件的符号链接）：跟随元数据取真实类型 / 大小；指向目录的
                // 符号链接 / 其它特殊文件 → 跳过（一律不递归，防循环）。
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
                let f = FolderFileHit { path, hits };
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
                for (j, &nb) in n.iter().enumerate().skip(1) {
                    if h[at + j].to_ascii_lowercase() != nb {
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
        // 一次性小写全文副本（std `to_ascii_lowercase`：仅折 ASCII 字母，长度不变）。
        let lower = content.to_ascii_lowercase();
        for (li, (line, lower_line)) in content.split('\n').zip(lower.split('\n')).enumerate() {
            if out.len() >= remaining {
                break;
            }
            if let Some(pos) = lower_line.find(q) {
                out.push(SearchLineHit {
                    line: li,
                    range: (doc_offset + pos)..(doc_offset + pos + q.len()),
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
