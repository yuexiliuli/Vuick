//! 兄弟文件导航：image / video 两个查看器各自实现的「枚举同目录兄弟文件 + 索引前后
//! 翻页 + 环绕」收敛为一个共享类型（[`FolderNav`]）。只收敛「清单 + 位置 + 步进」；
//! 打开/切换后的加载/解码/播放状态与事件入口留给调用方。

use std::path::{Path, PathBuf};

/// 兄弟文件导航。
///
/// 职责边界（收敛目标）：
/// - 目录扫描：[`Self::scan`]——同目录、扩展名白名单、文件名按 Windows 资源管理器「名称」
///   排序（[`Self::scan`] 描述）、定位当前项；
/// - 位置与步进：[`Self::step`]（`next`/`prev`）、`at_start`/`at_end`、`set_current`；
/// - 可选环绕：`wrap` 开 = 首/尾循环（`rem_euclid` 语义），关 = 边界步进返回 `None`。
///
/// **不**吸附调用方的加载/渲染状态机：图片的「级联预取 / B4 快滚 / 锁定信息条」、
/// 视频的「换代重开 / 播放器状态」都留在各自查看器，本类型只负责「当前在哪、下一步去哪」。
#[derive(Debug, Clone)]
pub struct FolderNav {
    /// 同目录兄弟文件清单（文件名按资源管理器「名称」排序，恒含当前项，恒非空——扫描
    /// 失败退化为单元素）。
    items: Vec<PathBuf>,
    /// 当前项在 `items` 中的下标（0 基；`set_current`/`step` 维护）。
    current: usize,
    /// 环绕开关：开 = 首→尾 / 尾→首循环；关 = 边界禁用（`step` 返回 `None`）。
    wrap: bool,
}

impl FolderNav {
    /// 直接构造（扫描请用 [`Self::scan`]）。`current` 越界时钳到末尾；默认环绕开。
    pub fn new(items: Vec<PathBuf>, current: usize) -> Self {
        let current = current.min(items.len().saturating_sub(1));
        Self {
            items,
            current,
            wrap: true,
        }
    }

    /// 设置环绕开关（默认开；关 = 边界禁用翻页）。当前两家查看器（image/video）都保持
    /// 环绕开，本方法暂无生产调用方——作为「边界禁用」扩展 API 存在，由下方纯函数单测
    /// 覆盖 wrap 开关语义（后续需要边界禁用的调用方可直接使用）。
    #[allow(dead_code)]
    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// 清单长度。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 当前项下标（0 基）。
    pub fn current(&self) -> usize {
        self.current
    }

    /// 当前项路径。
    pub fn current_path(&self) -> &Path {
        &self.items[self.current]
    }

    /// 下标取路径引用（越界 `None`）。
    pub fn get(&self, idx: usize) -> Option<&Path> {
        self.items.get(idx).map(PathBuf::as_path)
    }

    /// 下标取路径克隆（越界 `None`）。
    pub fn item(&self, idx: usize) -> Option<PathBuf> {
        self.items.get(idx).cloned()
    }

    /// 清单切片（只读辅助；生产代码用 [`Self::get`]/[`Self::item`] 取单条）。
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    /// 是否停在首个（环绕关闭时的滞后边界）。当前调用方均环绕开启，本方法暂无生产
    /// 调用方——作为「边界禁用」扩展 API 存在，由纯函数单测覆盖。
    #[allow(dead_code)]
    pub fn at_start(&self) -> bool {
        self.current == 0
    }

    /// 是否停在最后一个（环绕关闭时的前置边界）。同上，为「边界禁用」扩展 API。
    #[allow(dead_code)]
    pub fn at_end(&self) -> bool {
        self.current + 1 >= self.items.len()
    }

    /// 计算 `dir` 步进后的目标下标（日常 ±1 翻页，也接受任意 i64 做多步/跳转）。
    ///
    /// - `None`：无移动（单元素清单或 `dir == 0`），或环绕关闭且越过边界。
    /// - `Some(idx)`：目标下标；环绕开启时恒在 `[0, len)`（含首↔尾循环）。
    pub fn step(&self, dir: i64) -> Option<usize> {
        let n = self.items.len();
        if n <= 1 || dir == 0 {
            return None;
        }
        if !self.wrap {
            let v = self.current as i64 + dir;
            return (v >= 0 && v < n as i64).then_some(v as usize);
        }
        Some((self.current as i64).wrapping_add(dir).rem_euclid(n as i64) as usize)
    }

    /// 前进一（环绕开时尾→首）。无移动返回 `None`。
    pub fn next(&self) -> Option<usize> {
        self.step(1)
    }

    /// 后退一（环绕开时首→尾）。无移动返回 `None`。
    pub fn prev(&self) -> Option<usize> {
        self.step(-1)
    }

    /// 越界校验设置当前下标：越界返回 `None`（不改状态）；成功返回 `Some(idx)`。
    pub fn set_current(&mut self, idx: usize) -> Option<usize> {
        if idx >= self.items.len() {
            return None;
        }
        self.current = idx;
        Some(idx)
    }

    /// 扫描 `at` 所在目录的全部兄弟文件：`include` 做扩展名白名单过滤，文件名按 Windows
    /// 资源管理器「名称」排序（忽略大小写、数字块按数值、中文按拼音、标点排最前；Windows
    /// 走系统 collation，其它平台退数字自然序 + 字母序），并定位 `at` 为当前项。读取失败
    /// 或目录内无匹配 → 退化为「仅当前项」单元素清单。
    pub fn scan(at: &Path, include: impl Fn(&Path) -> bool) -> Self {
        let mut items = Vec::new();
        if let Some(dir) = at.parent()
            && let Ok(rd) = std::fs::read_dir(dir)
        {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_file() && include(&p) {
                    items.push(p);
                }
            }
        }
        if items.is_empty() {
            return Self::new(vec![at.to_path_buf()], 0);
        }
        items.sort_by(|a, b| compare_names(a, b));
        let current = items.iter().position(|p| p == at).unwrap_or(0);
        Self::new(items, current)
    }
}

// ---------------------------------------------------------------------------
// 文件名比较：复刻 Windows 资源管理器「名称」排序（供 [`FolderNav::scan`] 使用）。
//
// - Windows：对**原始文件名**（不剥任何字符）跑 kernel32 `CompareStringEx`，用用户默认
//   区域 collation + `NORM_IGNORECASE | SORT_DIGITSASNUMBERS`（忽略大小写、数字块按数值、
//   中文按拼音、标点排最前——与资源管理器「名称」视图同一套排序）；调用失败兜底下面对比。
// - 其它平台 / collation 失败：数字自然序 + 字母序兜底（无拼音）。
// - 只比较 file_name，不改 PathBuf 本体；collation 判等时按原始字节定序，恒为严格全序。
// ---------------------------------------------------------------------------

fn compare_names(a: &Path, b: &Path) -> std::cmp::Ordering {
    let ka = name_key(a);
    let kb = name_key(b);
    // collation 判等（Equal）时**继续**落到自然序/字节兜底，保证严格全序（稳定排序不残留输入序）。
    #[cfg(windows)]
    if let Some(ord) = collate_names(ka, kb)
        && ord != std::cmp::Ordering::Equal
    {
        return ord;
    }
    natural_cmp(ka, kb)
}

/// 比较键：优先文件名字串；`file_name` 为 None / 空串时退整路径（目录尾分隔等）。
fn name_key(p: &Path) -> &std::ffi::OsStr {
    p.file_name()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.as_os_str())
}

/// Windows 区域名称排序（资源管理器「名称」视图同款）。
#[cfg(windows)]
fn collate_names(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> Option<std::cmp::Ordering> {
    use std::os::windows::ffi::OsStrExt;
    let a16: Vec<u16> = a.encode_wide().collect();
    let b16: Vec<u16> = b.encode_wide().collect();
    // 资源管理器「名称」排序同款 flags：忽略大小写 + 数字块按数值。
    const NORM_IGNORECASE: u32 = 0x0000_0001;
    const SORT_DIGITSASNUMBERS: u32 = 0x0000_0008;
    // 返回 1=小于 2=等于 3=大于 0=失败。
    let r = unsafe {
        CompareStringEx(
            std::ptr::null(),
            NORM_IGNORECASE | SORT_DIGITSASNUMBERS,
            a16.as_ptr(),
            a16.len() as i32,
            b16.as_ptr(),
            b16.len() as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    match r {
        1 => Some(std::cmp::Ordering::Less),
        2 => Some(std::cmp::Ordering::Equal),
        3 => Some(std::cmp::Ordering::Greater),
        _ => None, // 失败：由调用方退自然序兜底
    }
}

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    #[allow(non_snake_case)]
    fn CompareStringEx(
        lpLocaleName: *const u16,
        dwCmpFlags: u32,
        lpString1: *const u16,
        cchCount1: i32,
        lpString2: *const u16,
        cchCount2: i32,
        lpVersionInformation: *mut core::ffi::c_void,
        lpReserved: *mut core::ffi::c_void,
        lParam: isize,
    ) -> i32;
}

/// 数字自然序 + 字母序兜底（非 Windows / collation 失败；纯函数便于单测）。
fn natural_cmp(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> std::cmp::Ordering {
    let sa = a.to_string_lossy();
    let sb = b.to_string_lossy();
    let mut ai = sa.chars().peekable();
    let mut bi = sb.chars().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na: Vec<char> =
                    std::iter::from_fn(|| ai.next_if(|c| c.is_ascii_digit())).collect();
                let nb: Vec<char> =
                    std::iter::from_fn(|| bi.next_if(|c| c.is_ascii_digit())).collect();
                let ord = cmp_digit_runs(&na, &nb);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            _ => {
                let x = ai.next().unwrap();
                let y = bi.next().unwrap();
                let ord = x.cmp(&y);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// 数字块按数值比较：去前导 0 后先比有效长度（长即大），再按字典序。
fn cmp_digit_runs(a: &[char], b: &[char]) -> std::cmp::Ordering {
    let sig_len = |v: &[char]| v.len() - v.iter().take_while(|c| **c == '0').count();
    let (la, lb) = (sig_len(a), sig_len(b));
    let ord = la.cmp(&lb);
    if ord != std::cmp::Ordering::Equal {
        return ord;
    }
    let sa = &a[a.len() - la..];
    let sb = &b[b.len() - lb..];
    sa.iter().cmp(sb.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// 纯函数步进：构造兄弟文件表，测 next/prev/wrap/边界与 at_start/at_end。
    #[test]
    fn step_wraps_cycles_and_respects_boundaries() {
        // 三元素、从中间 1 出发：环绕开启时前后都能走，且 尾→首 / 首→尾 循环。
        let nav = FolderNav::new(items(&["a", "b", "c"]), 1);
        assert_eq!(nav.len(), 3);
        assert_eq!(nav.current(), 1);
        assert!(!nav.at_start() && !nav.at_end());
        assert_eq!(nav.next(), Some(2));
        assert_eq!(nav.prev(), Some(0));
        // 环绕：尾→首。
        let at_tail = FolderNav::new(items(&["a", "b", "c"]), 2);
        assert!(at_tail.at_end());
        assert_eq!(at_tail.next(), Some(0), "环绕开：尾再向前回到首");
        // 环绕：首→尾。
        let at_head = FolderNav::new(items(&["a", "b", "c"]), 0);
        assert!(at_head.at_start());
        assert_eq!(at_head.prev(), Some(2), "环绕开：首再向后回到尾");

        // 环绕关：首/尾不再循环，越界步进返回 None。
        let no_wrap_head = FolderNav::new(items(&["a", "b", "c"]), 0).with_wrap(false);
        assert!(no_wrap_head.at_start());
        assert_eq!(no_wrap_head.prev(), None, "环绕关：首不可后退");
        assert_eq!(no_wrap_head.next(), Some(1));
        let no_wrap_tail = FolderNav::new(items(&["a", "b", "c"]), 2).with_wrap(false);
        assert!(no_wrap_tail.at_end());
        assert_eq!(no_wrap_tail.next(), None, "环绕关：尾不可前进");
        assert_eq!(no_wrap_tail.prev(), Some(1));

        // step 任意 i64 多步：环绕开时等价 rem_euclid。
        let nav = FolderNav::new(items(&["a", "b", "c"]), 1);
        assert_eq!(nav.step(3), Some(1), "步进整圈回到原位");
        assert_eq!(nav.step(-2), Some(2), "反向多步取模");

        // 单元素 / dir==0：无移动。
        let single = FolderNav::new(items(&["a"]), 0);
        assert_eq!(single.step(1), None, "单元素不可步进");
        assert_eq!(single.step(-1), None);
        assert_eq!(nav.step(0), None, "零步无移动");

        // set_current：越界拒绝、合法更新。
        let mut nav = FolderNav::new(items(&["a", "b", "c"]), 0);
        assert_eq!(nav.set_current(1), Some(1));
        assert_eq!(nav.current(), 1);
        assert_eq!(nav.set_current(3), None, "越界拒绝，不改状态");
        assert_eq!(nav.current(), 1);
        assert_eq!(nav.current_path(), Path::new("b"));
        assert_eq!(nav.get(2), Some(Path::new("c")));
        assert_eq!(nav.item(2), Some(PathBuf::from("c")));
        assert_eq!(nav.get(9), None);
    }

    /// 扫描：扩展名白名单（谓词只管准不准，具体扩展名由调用方定）、升序、定位当前、
    /// 空目录/无匹配退化单元素。用真实临时目录验证。
    #[test]
    fn scan_filters_sorts_and_locates_current_with_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-folder-nav-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let b = mk("b.jpg");
        let a = mk("a.jpg");
        let c = mk("c.jpeg");
        let _txt = mk("note.txt"); // 白名单外，应被排除
        let mut list: Vec<String> = Vec::new();
        let nav = FolderNav::scan(&b, |p| {
            matches!(p.extension().and_then(|e| e.to_str()), Some(e) if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
        });
        for p in nav.items() {
            list.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(list, vec!["a.jpg", "b.jpg", "c.jpeg"]);
        assert_eq!(nav.current(), 1);
        assert_eq!(nav.current_path(), b);
        assert!(nav.items().contains(&a) && nav.items().contains(&c), "a/c 应在清单内");

        // 空目录 / 无匹配 → 退化「仅当前项」单元素。
        let empty_dir = dir.join("sub");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let orphan = empty_dir.join("orphan.png");
        let fallback = FolderNav::scan(&orphan, |_| true);
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback.current(), 0);
        assert_eq!(fallback.current_path(), orphan);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 资源管理器「名称」排序：数字块按数值（1 < 2 < 10 < 343），hash 文件名同理。
    #[test]
    fn name_sort_numbers_natural() {
        let mut names = vec!["343.png", "10.png", "2.png", "1.png", "a.png"];
        names.sort_by(|a, b| compare_names(Path::new(a), Path::new(b)));
        assert_eq!(names, vec!["1.png", "2.png", "10.png", "343.png", "a.png"]);

        // 数字开头 hash 文件名按数值：01e4(1) < 2c75(2) < 0343(343)
        let mut hashes = vec![
            "0343ab9a1cf156a308049ea5800f4dc3274403215.jpg",
            "2c750dd6e7b84b7c1ce00e26fce4ffa1346174401.png",
            "01e4baef7eb46fc553ea0353eb25a3a91033035079.jpg",
        ];
        hashes.sort_by(|a, b| compare_names(Path::new(a), Path::new(b)));
        assert_eq!(
            hashes[0],
            "01e4baef7eb46fc553ea0353eb25a3a91033035079.jpg",
            "01e4baef 数字值最小应排最前"
        );
    }

    /// 忽略大小写：collation 判等后按原始字节 tie-break（大写字母码点小在前）。
    #[test]
    fn name_sort_case_insensitive_with_byte_tie_break() {
        let mut names = vec!["b.jpg", "B.jpg"];
        names.sort_by(|a, b| compare_names(Path::new(a), Path::new(b)));
        assert_eq!(names, vec!["B.jpg", "b.jpg"]);
    }

    /// 标点排最前：弯引号（U+201C）开头的名字按资源管理器名称序排在数字文件名之前。
    #[test]
    fn name_sort_leading_punctuation_before_digits() {
        let mut names = vec![
            "01e4baef7eb46fc553ea0353eb25a3a91033035079.jpg",
            "\u{201c}爱丽丝\u{201d}壁纸.png",
            "a.jpg",
        ];
        names.sort_by(|a, b| compare_names(Path::new(a), Path::new(b)));
        assert_eq!(
            names[0],
            "\u{201c}爱丽丝\u{201d}壁纸.png",
            "弯引号开头的名字应排最前（与资源管理器一致）"
        );
    }

    /// scan 对真实临时目录按名称序排序并正确定位当前项（数字按数值，跨平台成立）。
    #[test]
    fn scan_uses_name_sort_in_natural_order() {
        let dir = std::env::temp_dir().join(format!(
            "gpui-editor-folder-nav-name-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        };
        let _ = mk("b10.png");
        let _ = mk("b1.png");
        let _ = mk("b2.png");
        let target = mk("b3.png");
        let nav = FolderNav::scan(&target, |_| true);
        let list: Vec<String> = nav
            .items()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(list, vec!["b1.png", "b2.png", "b3.png", "b10.png"]);
        assert_eq!(nav.current(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手工验证（依赖 zh-CN 区域 collation 的中文拼音序）：爱(ai) < 迪(di) < 咕(gu)。
    #[test]
    #[ignore = "依赖本机 zh-CN 区域 collation"]
    fn han_pinyin_order_zh_cn() {
        let mut v = vec!["咕嘎可爱.gif", "爱丽丝壁纸.png", "迪迦.jpg"];
        v.sort_by(|a, b| compare_names(Path::new(a), Path::new(b)));
        assert_eq!(v, vec!["爱丽丝壁纸.png", "迪迦.jpg", "咕嘎可爱.gif"]);
    }

    /// 手工验证（依赖真实测试目录 + zh-CN collation）：复刻资源管理器名称序后，
    /// “爱丽丝”壁纸.png 应排第 0 位、下一页恰为 01e4baef….jpg（用户报的期望顺序）。
    #[test]
    #[ignore = "依赖真实目录 C:\\Users\\yuexi\\Pictures\\test 与 zh-CN 区域 collation"]
    fn explorer_name_sort_on_real_test_folder() {
        let dir = std::path::Path::new(r"C:\Users\yuexi\Pictures\test");
        if !dir.is_dir() {
            eprintln!("SKIP: missing {dir:?}");
            return;
        }
        let is_img = |p: &Path| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff"
                )
            })
        };
        let mut items: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_img(p))
            .collect();
        items.sort_by(|a, b| compare_names(a, b));
        let names: Vec<String> = items
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        eprintln!(
            "资源管理器名称序（{} 项）:\n{}",
            names.len(),
            names.join("\n")
        );
        assert_eq!(
            names[0],
            "\u{201c}爱丽丝\u{201d}壁纸.png",
            "弯引号开头的“爱丽丝”壁纸.png 应排第 0 位"
        );
        assert_eq!(
            names[1],
            "01e4baef7eb46fc553ea0353eb25a3a91033035079.jpg",
            "爱丽丝下一页应为 01e4baef….jpg"
        );
    }
}
