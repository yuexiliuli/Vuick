//! 兄弟文件导航：image / video 两个查看器各自实现的「枚举同目录兄弟文件 + 索引前后
//! 翻页 + 环绕」收敛为一个共享类型（[`FolderNav`]）。只收敛「清单 + 位置 + 步进」；
//! 打开/切换后的加载/解码/播放状态与事件入口留给调用方。

use std::path::{Path, PathBuf};

/// 兄弟文件导航。
///
/// 职责边界（收敛目标）：
/// - 目录扫描：[`Self::scan`]——同目录、扩展名白名单、文件名升序、定位当前项；
/// - 位置与步进：[`Self::step`]（`next`/`prev`）、`at_start`/`at_end`、`set_current`；
/// - 可选环绕：`wrap` 开 = 首/尾循环（`rem_euclid` 语义），关 = 边界步进返回 `None`。
///
/// **不**吸附调用方的加载/渲染状态机：图片的「级联预取 / B4 快滚 / 锁定信息条」、
/// 视频的「换代重开 / 播放器状态」都留在各自查看器，本类型只负责「当前在哪、下一步去哪」。
#[derive(Debug, Clone)]
pub struct FolderNav {
    /// 同目录兄弟文件清单（文件名升序，恒含当前项，恒非空——扫描失败退化为单元素）。
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

    /// 扫描 `at` 所在目录的全部兄弟文件：`include` 做扩展名白名单过滤，文件名升序，
    /// 并定位 `at` 为当前项。读取失败或目录内无匹配 → 退化为「仅当前项」单元素清单。
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
        items.sort();
        let current = items.iter().position(|p| p == at).unwrap_or(0);
        Self::new(items, current)
    }
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
}
