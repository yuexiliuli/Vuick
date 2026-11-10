// 侧边栏搜索匹配/扫描语义边界（纯逻辑单测，不进 gpui 窗口）：
// `find_ci` 大小写折叠、首个命中、UTF-8 字节区间；`scan_line_hits` 行偏移、
// 单行多命中、上限截断、空文档/尾换行、极长行。保证优化不漂移命中语义。

use shell_scan::{find_ci, folder_scan_collect, scan_line_hits, FolderScanUpdate};
use config::limits::SEARCH_RESULT_CAP;

#[test]
fn find_ci_ascii_case_insensitive_substring() {
    // 大小写混合正文 + 小写查询词（生产经 normalize_search_query 归一化）：首个命中的字节区间
    assert_eq!(find_ci("Hello World", "world"), Some(6..11));
    assert_eq!(find_ci("Hello World", "hell"), Some(0..4));
    assert_eq!(find_ci("aaaXaaa", "x"), Some(3..4));
    // 无命中 / 空 needle / needle 长于正文
    assert_eq!(find_ci("Hello World", "xyz"), None);
    assert_eq!(find_ci("abc", ""), None);
    assert_eq!(find_ci("ab", "abc"), None);
}

#[test]
fn find_ci_mixed_case_haystack_lowercase_needle() {
    // 查询词已小写；正文任意大小写均命中（ASCII 折叠）
    assert_eq!(find_ci("HeLLo WoRLD", "world"), Some(6..11));
    // 命中词内部大小写混合（C↔c、D↔d 折叠）
    assert_eq!(find_ci("AbCdEfG", "bcd"), Some(1..4));
    assert_eq!(find_ci("aBcDeFg", "cdef"), Some(2..6));
}

#[test]
fn find_ci_non_ascii_unicode_fold() {
    // 非 ASCII 查询走字符级大小写折叠（É→é），非整串小写语义；字节区间按 UTF-8（É=2 字节）
    assert_eq!(find_ci("ÉDITION PARIS", "édition"), Some(0..8));
    // 中文查询（无大小写）
    assert_eq!(find_ci("他是编辑人员", "编辑"), Some(6..12));
    assert_eq!(find_ci("他是编辑人员", "不在"), None);
    // 中文正文 + ASCII 查询词（memchr 快路径仍只命中 ASCII 段）
    assert_eq!(find_ci("中文abc中文", "abc"), Some(6..9));
}

#[test]
fn find_ci_very_long_line_matches_at_end() {
    // 极长行：命中在行尾（memchr 快路径扫完整行，字节区间按行礼）
    let mut line = String::from("a");
    line.push_str(&"x".repeat(100_000));
    line.push_str("target");
    let r = find_ci(&line, "target").expect("应在长行尾部命中");
    assert_eq!(r, 100_001..100_007);
    assert_eq!(&line[r], "target");
}

#[test]
fn scan_line_hits_utf8_byte_range() {
    // 中文字节偏移正确：'中文内容 ' = 4×3 + 1 = 13 字节，'abc' 落 13..16
    let content = "中文内容 abc\n其他行\n";
    let mut out = Vec::new();
    scan_line_hits(content, "abc", SEARCH_RESULT_CAP, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].line, 0);
    assert_eq!(out[0].range, 13..16);
    assert_eq!(out[0].snippet, "中文内容 abc");
}

#[test]
fn scan_line_hits_single_line_multi_hit_keeps_first() {
    // 单行多个命中只收首个（现语义），区间为首个命中
    let content = "aXaY\n";
    let mut out = Vec::new();
    scan_line_hits(content, "a", SEARCH_RESULT_CAP, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].line, 0);
    assert_eq!(out[0].range, 0..1);
    assert_eq!(out[0].snippet, "aXaY");
}

#[test]
fn scan_line_hits_cross_line_offsets() {
    // 跨行 doc_offset 累积（行长 + 换行 1 字节），行号/区间一一对应
    let content = "aa\nbbb\nccc aaa\n";
    let mut out = Vec::new();
    scan_line_hits(content, "aa", SEARCH_RESULT_CAP, &mut out);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].line, 0);
    assert_eq!(out[0].range, 0..2);
    assert_eq!(out[1].line, 2);
    assert_eq!(out[1].range, 11..13);
    assert_eq!(out[1].snippet, "ccc aaa");
}

#[test]
fn scan_line_hits_cap_truncates_hits() {
    // 命中多行时按 remaining 上限截断（命中行仍是开头的行）
    let content = "x\nx\nx\nx\nx\n";
    let mut out = Vec::new();
    scan_line_hits(content, "x", 3, &mut out);
    assert_eq!(out.len(), 3);
    assert_eq!((out[0].line, out[1].line, out[2].line), (0, 1, 2));
}

#[test]
fn scan_line_hits_empty_doc_and_no_match() {
    let mut out = Vec::new();
    scan_line_hits("", "abc", SEARCH_RESULT_CAP, &mut out);
    assert!(out.is_empty(), "空文档不应有命中");

    let mut out = Vec::new();
    scan_line_hits("正常内容\n没有目标\n", "zzz", SEARCH_RESULT_CAP, &mut out);
    assert!(out.is_empty(), "无匹配词不应有命中");
}

#[test]
fn scan_line_hits_snippet_trims_leading_trailing_space() {
    // 片段去掉首尾空白；字节区间仍按原文（含空白）
    let content = "  spaced needle  \n";
    let mut out = Vec::new();
    scan_line_hits(content, "needle", SEARCH_RESULT_CAP, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].range, 9..15);
    assert_eq!(out[0].snippet, "spaced needle");
}

#[test]
fn scan_line_hits_zero_remaining_is_noop() {
    // 外层配额已满（remaining=0）→ 不扫
    let mut out = Vec::new();
    scan_line_hits("needle\n", "needle", 0, &mut out);
    assert!(out.is_empty());
}

#[test]
fn scan_line_hits_non_ascii_query_uses_char_fold() {
    // 非 ASCII 查询词走字符级大小写折叠分支（É→é），字节区间按 UTF-8
    let mut out = Vec::new();
    scan_line_hits("ÉDITION PARIS\n其它", "édition", SEARCH_RESULT_CAP, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].line, 0);
    assert_eq!(out[0].range, 0..8);
    assert_eq!(out[0].snippet, "ÉDITION PARIS");
}

#[test]
fn scan_line_hits_ascii_copy_matches_old_semantics() {
    // 新 ASCII 分支（一次小写副本 + str::find）与旧「逐行 to_lowercase + contains」命中一致
    let corpus = "Line of TEXT with NeedleMark and 中文 mixed 内容\n再一行 ｎｅｅｄｌｅ 全角\nplain text\n";
    for q in ["needlemark", "text", "plain", "中文", "全角"] {
        let mut out = Vec::new();
        scan_line_hits(corpus, q, SEARCH_RESULT_CAP, &mut out);
        let old = corpus
            .split('\n')
            .filter(|l| !l.is_empty())
            .filter(|l| l.to_lowercase().contains(q))
            .count()
            .min(SEARCH_RESULT_CAP);
        assert_eq!(
            out.len().min(SEARCH_RESULT_CAP),
            old,
            "查询 {q}：新旧命中数应一致"
        );
    }
}

/// 文件夹级搜索：目录级符号链接（Unix symlink / Windows 目录联接）**不递归**，防
/// 「循环引用 → 扫描无限循环」。（回归：`folder_scan_collect` 用 `entry.file_type()`
/// （不跟随）判目录；若用 `metadata`（跟随）判，联接指向自身会无限入栈挂死。）
#[test]
fn folder_scan_skips_symlinked_dirs_prevents_cycle() {
    use futures::{SinkExt as _, StreamExt as _};
    let base = std::env::temp_dir().join(format!("gpui-fs-sym-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // 一个命中文件 + 一个「循环指向自身」的目录链接：若跟随递归 → 无限扫描
    std::fs::write(base.join("hit.txt"), "needle here\n").unwrap();
    let link = base.join("loop");
    if !create_dir_link(&link, &base) {
        // 环境不支持创建链接 → 跳过（保持非空运行，Windows 目录联接 / Unix 符号链接通常可用）
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let (mut tx, mut rx) = futures::channel::mpsc::channel::<FolderScanUpdate>(4);
    let mut total = 0usize;
    let truncated = futures::executor::block_on(folder_scan_collect(
        &base,
        "needle",
        tx.clone(),
        &std::path::PathBuf::from("__nonexistent_session__").as_path(),
        &mut total,
        SEARCH_RESULT_CAP,
    ));
    assert!(!truncated, "小目录不应截断");
    assert_eq!(total, 1, "应命中 hit.txt 且不因循循环重复计数");
    // 模拟生产端：扫描结束后补发 Done（`start_folder_scan` 的真实姿势），再释放全部 sender
    futures::executor::block_on(tx.send(FolderScanUpdate::Done { truncated })).expect("send Done");
    drop(tx);
    let mut blocks = 0usize;
    let mut done = false;
    while let Some(u) = futures::executor::block_on(rx.next()) {
        blocks += 1;
        if matches!(u, FolderScanUpdate::Done { .. }) {
            done = true;
        }
    }
    assert!(done, "应收到 Done 块（扫描正常结束而非挂死）");
    assert_eq!(blocks, 2, "应恰好 1 个 File 块 + 1 个 Done 块");
    let _ = std::fs::remove_dir_all(&base);
}

/// Windows：`mklink /J` 目录联接（无需管理员权限）；失败返回 false。
#[cfg(windows)]
fn create_dir_link(link: &std::path::Path, target: &std::path::Path) -> bool {
    std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Unix：目录符号链接；失败返回 false。
#[cfg(not(windows))]
fn create_dir_link(link: &std::path::Path, target: &std::path::Path) -> bool {
    std::os::unix::fs::symlink_dir(target, link).is_ok()
}
