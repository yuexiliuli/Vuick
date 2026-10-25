//! 编辑区内查找（Ctrl+F / F3 / Shift+F3）：find 状态机 / 匹配表 / 导航。
//!
//! 与视图的边界（职责清晰，禁止把逻辑堆进 view）：本模块只维护「查询词、匹配表、
//! 当前命中、上限截断、缓存键」等**纯状态与纯函数**；绘制高亮层、输入框接线、防抖
//! 调度全部在 view 层（[`crate::view::mod`] / [`crate::view::render`]）。
//!
//! 坐标约定：匹配表用 UTF-8 **字节偏移**（与 [`Buffer`] 全链路一致）。中文/emoji 的
//! 串位风险由「逐行 slice + 行首字节偏移累加」天然消除——行内匹配始终落在字符边界上，
//! 与光标/选区/滚动共用同一套字节坐标，无需额外 UTF-16 换算。
//!
//! 语义要点（对齐行为表）：
//! - 大小写**不敏感**（本阶段无开关），字面文本查找（`regex::escape` 转义）；
//! - **非重叠**匹配（正则 `find_iter` 天然向前推进）；
//! - 匹配上限 [`FIND_MATCH_CAP`]：到上限**立即停止收集**（先收满/到限即刷新的「首 N 个
//!   快显」），状态显示截断版「i/N+」，导航只在上限内回绕；
//! - 文本插删 / 撤销重做后按 `(buffer 版本, 查询词)` 失效重算（防抖同一条路径）；
//! - 光标移动不重算匹配表（缓存只随版本/词失效），只重定位当前命中。

use std::ops::Range;

use crate::buffer::Buffer;

/// 匹配收集上限：达到即停止收集（`capped=true`），导航只在上限内回绕。
/// 200 = 首屏高频查询的实用上限：再多匹配对「跳下一个」无增益且拖慢收集/内存/绘制。
pub const FIND_MATCH_CAP: usize = 200;

/// 编辑区查找状态机（per-`CodeEditor` 一份；CodeEditor 本身 per-tab → find 状态随 tab
/// 实体独立释放，切换/关闭 tab 无残留高亮/句柄）。
#[derive(Default)]
pub struct FindOverlay {
    /// find 条是否打开（编辑区顶部浮层）。
    pub open: bool,
    /// 是否绘制高亮（Esc 关条 → false；F3 复用导航会重新拉起）。
    pub visible: bool,
    /// 当前查询词（与输入框文本同步；Esc 关条后保留供 F3 复用）。
    pub query: String,
    /// 最近一次查询（每次成功收集后更新；F3 无此则不动）。
    pub last_query: String,
    /// 匹配表（全文字节区间，升序、非重叠）；达到上限时只含前 [`FIND_MATCH_CAP`] 个。
    pub matches: Vec<Range<usize>>,
    /// 当前命中在 `matches` 的下标。
    pub current: Option<usize>,
    /// 是否因达到上限而截断（状态显示截断版「i/N+」）。
    pub capped: bool,
    /// 匹配表对应的 buffer 版本（文本插删/撤销重做后失效重算）。
    pub matches_version: u64,
    /// 匹配表对应的查询词（与 `query` 一致性判别，防「同版本不同词」命中旧表）。
    pub matches_query: String,
    /// 防抖任务代号（过期任务落地被丢弃，陈旧结果不覆盖新输入）。
    pub scan_epoch: u64,
    /// 待重算标记（输入/编辑置位，统一刷新路径消费）。
    pub dirty: bool,
}

impl FindOverlay {
    /// 是否处于「可绘制高亮」态（关条/无匹配 → false，零绘制开销）。
    pub fn is_active(&self) -> bool {
        self.visible && !self.matches.is_empty()
    }

    /// 计数文案（紧凑卡片版）：`i/N` / `无匹配` / `i/N+`（`N+` = 达到
    /// [`FIND_MATCH_CAP`] 上限截断，总数为 N 而实为更多）。
    pub fn status(&self) -> String {
        // 当前命中序（1 基）：`current` 越界（人工构造/陈旧）时退回 1，防显示越界序。
        let i = self
            .current
            .filter(|c| *c < self.matches.len())
            .map(|c| c + 1)
            .unwrap_or(1);
        if self.capped {
            format!("{i}/{}+", FIND_MATCH_CAP)
        } else if self.matches.is_empty() {
            "无匹配".to_string()
        } else {
            format!("{i}/{}", self.matches.len())
        }
    }

    /// 打开 find 条：如果当前无查询词则带入最近一次查询（有则保持已输入内容）。
    pub fn open_bar(&mut self) {
        self.open = true;
        self.visible = true;
        if self.query.is_empty() {
            self.query.clone_from(&self.last_query);
        }
        self.current = None;
        self.dirty = true;
    }

    /// 关闭 find 条：移除全部高亮、清空匹配表，但**保留 `last_query`** 供 F3/Shift+F3 复用。
    pub fn close_bar(&mut self) {
        self.open = false;
        self.visible = false;
        self.matches.clear();
        self.current = None;
        self.capped = false;
        self.dirty = false;
    }

    /// 输入词变化 / buffer 编辑失效：只置脏 + 起新代号（统一刷新路径重建匹配，
    /// 一处脏标记，不散点直调重算）。
    pub fn invalidate(&mut self) {
        self.dirty = true;
        self.current = None;
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
    }

    /// 取走一次「防抖已到期」的扫描请求：返回其代号（调用方据此判定落地是否过期）。
    pub fn take_scan_request(&mut self) -> Option<u64> {
        if self.dirty {
            self.dirty = false;
            Some(self.scan_epoch)
        } else {
            None
        }
    }

    /// 落地一批匹配（在给定 buffer 版本 + 查询词下）：替换匹配表并定位当前命中。
    /// `focus_first` = true（输入后首扫）时把首个匹配设为当前。
    pub fn land(
        &mut self,
        buffer_version: u64,
        query: String,
        matches: Vec<Range<usize>>,
        capped: bool,
        focus_first: bool,
    ) {
        self.matches = matches;
        self.capped = capped;
        self.matches_version = buffer_version;
        self.matches_query.clone_from(&query);
        self.last_query.clone_from(&query);
        self.visible = true;
        self.current = if focus_first && !self.matches.is_empty() {
            Some(0)
        } else {
            self.current.filter(|c| *c < self.matches.len())
        };
    }

    /// 匹配表与给定 `(buffer 版本, 查询词)` 是否一致（失效判定）。
    pub fn fresh_for(&self, buffer_version: u64, query: &str) -> bool {
        !self.dirty
            && self.matches_version == buffer_version
            && self.matches_query == query
    }

    /// 下一个 / 上一个匹配（跨循环回绕）；无匹配返回 `None`（不动作、不回绕）。
    /// 返回 `(新当前下标, 匹配起点字节)`。
    pub fn step(&mut self, next: bool) -> Option<(usize, usize)> {
        let n = self.matches.len();
        if n == 0 {
            self.current = None;
            return None;
        }
        let cur = self.current.unwrap_or(0);
        let idx = if next {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        self.current = Some(idx);
        Some((idx, self.matches[idx].start))
    }

    /// 某行内的查找高亮（行本地字节区间 → (区间, 是否当前命中)）；未可见 / 无匹配
    /// / 空行 → 空。匹配表升序 → 行内查询走二分 + 顺序推进，只遍历本行命中。
    pub fn row_highlights(&self, buffer: &Buffer, line: usize) -> Vec<(Range<usize>, bool)> {
        if !self.visible || self.matches.is_empty() {
            return Vec::new();
        }
        let ls = buffer.line_start(line);
        let le = buffer.line_end(line);
        if ls >= le {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = self.matches.partition_point(|m| m.start < ls);
        while i < self.matches.len() {
            let m = &self.matches[i];
            if m.start >= le {
                break;
            }
            let s = m.start.max(ls);
            let e = m.end.min(le);
            if e > s {
                out.push((s - ls..e - ls, self.current == Some(i)));
            }
            i += 1;
        }
        out
    }
}

/// 纯函数：整篇查找匹配区间（升序、非重叠、上限截断）。
///
/// 行级扫描（find 输入框是单行，查询词不含换行，逐行匹配与整篇匹配等价）：
/// - 不物化全文（ropey `slice_line` 逐行取，内存与单行成正比）；
/// - 到上限**立即停止**（大文档「先收满/到限即刷新」：首 N 个匹配不因全文收集而等太久）；
/// - 每行内用 `find_iter`（非重叠、UTF-8 字节边界安全）。
pub fn compute_matches(buffer: &Buffer, query: &str, cap: usize) -> (Vec<Range<usize>>, bool) {
    if query.is_empty() || query.contains(['\n', '\r']) {
        return (Vec::new(), false);
    }
    let Ok(re) = regex::Regex::new(&format!("(?i){}", regex::escape(query))) else {
        return (Vec::new(), false);
    };
    let mut matches = Vec::with_capacity(64);
    let mut capped = false;
    'outer: for row in 0..buffer.line_count() {
        let ls = buffer.line_start(row);
        let line = buffer.line_text(row);
        for m in re.find_iter(line.as_ref()) {
            matches.push(ls + m.start()..ls + m.end());
            if matches.len() >= cap {
                capped = true;
                break 'outer;
            }
        }
    }
    (matches, capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Buffer {
        Buffer::new(s)
    }

    #[test]
    fn compute_basic_case_insensitive_non_overlap() {
        let b = buf("Hello hello HELLO\nfoo hello");
        let (m, cap) = compute_matches(&b, "hello", FIND_MATCH_CAP);
        assert!(!cap);
        let starts: Vec<usize> = m.iter().map(|r| r.start).collect();
        // "Hello"@0、"hello"@6、"HELLO"@12（行 0 各 5 字节）、下一行 "foo hello" 的
        // "hello"@22（行首 22 = "Hello hello HELLO\n" 18B + "foo " 4B）。
        assert_eq!(starts, vec![0, 6, 12, 22]);
        assert!(m.iter().all(|r| r.start < r.end), "非空匹配");
    }

    #[test]
    fn compute_literal_special_chars_escaped() {
        // 正则特殊字符按字面查找（regex::escape 转义），不误命中其他位置。
        let b = buf("a+b a b a+b c+d");
        let (m, _) = compute_matches(&b, "a+b", FIND_MATCH_CAP);
        assert_eq!(m, vec![0..3, 8..11]);
    }

    #[test]
    fn compute_cap_truncates() {
        // 300 个匹配 > cap 200 → 截断 + capped。
        let text = "x".repeat(600);
        let b = buf(&format!("{text}"));
        let (m, cap) = compute_matches(&b, "xx", 200);
        assert_eq!(m.len(), 200);
        assert!(cap, "达到上限应截断");
    }

    #[test]
    fn compute_empty_and_newline_query() {
        let b = buf("abc\nabc");
        assert_eq!(compute_matches(&b, "", FIND_MATCH_CAP), (Vec::new(), false));
        assert_eq!(compute_matches(&b, "a\nb", FIND_MATCH_CAP), (Vec::new(), false), "含换行查询不匹配");
    }

    #[test]
    fn compute_cjk_and_emoji_byte_offsets() {
        // 中文/emoji 行内匹配的字节区间必须落在字符边界上（不串位）。
        let b = buf("中文查找查找🎉\n下一行查找");
        let (m, _) = compute_matches(&b, "查找", FIND_MATCH_CAP);
        // 行 0 "中文查找查找🎉"："中"3B+"文"3B=6，两次「查找」各 6B → 6..12、12..18；
        // 行 0 共 22B + "\n"=23 → 行 1 "下一行查找"（"下一行" 9B + 行首 23 = 32）→ 32..38。
        let expect = vec![6..12, 12..18, 32..38];
        assert_eq!(m, expect);
        for r in &m {
            let s = b.text_in_range(r.clone());
            assert_eq!(s.to_string(), "查找");
        }
    }

    #[test]
    fn step_wraps_around() {
        let mut f = FindOverlay {
            matches: vec![0..2, 5..7, 10..12],
            current: Some(0),
            ..Default::default()
        };
        assert_eq!(f.step(true), Some((1, 5)));
        assert_eq!(f.step(true), Some((2, 10)));
        assert_eq!(f.step(true), Some((0, 0)), "末尾回绕到开头");
        assert_eq!(f.step(false), Some((2, 10)), "开头回绕到末尾");
        assert_eq!(f.step(false), Some((1, 5)));
    }

    #[test]
    fn step_no_match_returns_none() {
        let mut f = FindOverlay::default();
        assert_eq!(f.step(true), None);
        assert_eq!(f.current, None);
    }

    #[test]
    fn row_highlights_local_and_current_flag() {
        let b = buf("aaa bbb aaa\nccc aaa");
        let mut f = FindOverlay {
            visible: true,
            matches: vec![0..3, 8..11, 16..19],
            current: Some(1),
            ..Default::default()
        };
        // 行 0："aaa bbb aaa"（行内本地区间 0..9）→ 命中 0..3、8..11-8=8..11 → 本地 8..11
        let r0 = f.row_highlights(&b, 0);
        assert_eq!(r0, vec![(0..3, false), (8..11, true)], "行 0 两命中、第 2 个为当前");
        // 行 1（ls=12）："ccc aaa" → 命中 16..19 → 本地 4..7、非当前
        let r1 = f.row_highlights(&b, 1);
        assert_eq!(r1, vec![(4..7, false)]);
        // 空行 / 越界行 → 空
        assert!(f.row_highlights(&b, 3).is_empty(), "越界行无高亮");
        // 未可见 → 空
        f.visible = false;
        assert!(f.row_highlights(&b, 0).is_empty());
    }

    #[test]
    fn status_formats() {
        let mut f = FindOverlay::default();
        assert_eq!(f.status(), "无匹配");
        f.matches = vec![0..1, 2..3, 4..5];
        f.current = Some(0);
        assert_eq!(f.status(), "1/3");
        f.current = Some(2);
        assert_eq!(f.status(), "3/3");
        f.matches = vec![0..1];
        f.capped = true;
        assert_eq!(f.status(), format!("1/{}+", FIND_MATCH_CAP));
    }

    #[test]
    fn land_and_fresh_for() {
        let mut f = FindOverlay::default();
        f.land(3, "ab".into(), vec![0..2, 4..6], false, true);
        assert_eq!(f.last_query, "ab");
        assert_eq!(f.current, Some(0));
        assert!(f.fresh_for(3, "ab"), "同版本同词 → 新鲜");
        assert!(!f.fresh_for(4, "ab"), "版本变了 → 失效");
        assert!(!f.fresh_for(3, "ac"), "词变了 → 失效");
        f.invalidate();
        assert!(!f.fresh_for(3, "ab"), "置脏 → 失效");
        assert!(f.take_scan_request().is_some(), "脏标记可取走扫描请求");
        assert!(f.take_scan_request().is_none(), "取走即清");
    }

    #[test]
    fn close_bar_preserves_last_query_and_clears() {
        let mut f = FindOverlay {
            query: "ab".into(),
            last_query: "ab".into(),
            matches: vec![0..2],
            current: Some(0),
            visible: true,
            open: true,
            ..Default::default()
        };
        f.close_bar();
        assert!(!f.open && !f.visible);
        assert!(f.matches.is_empty());
        assert_eq!(f.last_query, "ab", "last_query 保留供 F3 复用");
        assert_eq!(f.query, "ab", "query 保留（F3 复用词源）");
    }
}
