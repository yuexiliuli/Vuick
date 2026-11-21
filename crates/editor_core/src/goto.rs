//! 编辑区内跳转行（Ctrl+G）：goto 状态机 / 解析 / clamp / 内联提示文案。
//!
//! 与视图的边界（职责清晰，同 find.rs）：本模块只维护「打开、输入缓存、最近一跳、
//! 防抖代号、待跳转标记、内联提示文案」等**纯状态与纯函数**；输入框实体、防抖调度、
//! 卡片绘制、跳转落地（scroll_to_line + set_selection_range）全部在 view 层
//! （[`crate::view::mod`] / [`crate::view::render`]）。
//!
//! 语义要点（对齐行为表）：
//! - 输入「行[:列]」1 基，`行`与`行:列`都支持；非法（非数字 / 多个冒号 / 空段）→ `None`；
//! - clamp：0 / 负 → 1、超行数 → 末行、列 < 1 → 1（列超行宽由 view 的
//!   `offset_for_line_col` 钳制，本模块不做行宽判断）；
//! - 空文档 / 大文档未挂载：不跳不崩（view 侧沿既有空文档防护）。

/// 纯函数：解析跳转输入「行[:列]」（均为 1 基）：行可省略列；非法（非数字 / 多个
/// 冒号 / 空段）→ `None`。正负与 0 由调用方 clamp（从 shell palette 迁入，语义不变）。
pub fn parse_goto(s: &str) -> Option<(i64, Option<i64>)> {
    let (row_s, col_s) = match s.split_once(':') {
        Some((r, c)) => (r, Some(c)),
        None => (s, None),
    };
    if col_s.is_some_and(|c| c.contains(':')) {
        return None; // 多个冒号 → 非法
    }
    let row = row_s.trim().parse::<i64>().ok()?;
    let col = match col_s {
        Some(c) => Some(c.trim().parse::<i64>().ok()?),
        None => None,
    };
    Some((row, col))
}

/// 一次跳转意图解析 + clamp 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotoTarget {
    /// 目标行（1 基，已 clamp 到 `[1, line_count]`）。
    pub row1: usize,
    /// 目标列（1 基，clamp 后；未给列按 1）。
    pub col1: usize,
    /// 行/列是否发生了钳制（0 / 负 / 超行数 / 列 < 1）。
    pub clamped: bool,
}

/// 解析 + clamp（语义对齐旧 shell 提交逻辑）：空输入 / 非法返回 `None`；
/// 0 / 负行 → 1、超行数 → 末行、列 < 1 → 1（列超行宽由 view 的 `offset_for_line_col`
/// 钳制）。
pub fn parse_and_clamp(input: &str, line_count: usize) -> Option<GotoTarget> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (row_i, col_i) = parse_goto(trimmed)?;
    let mut clamped = false;
    let row1 = if row_i < 1 {
        clamped = true;
        1
    } else if row_i as usize > line_count {
        clamped = true;
        line_count.max(1)
    } else {
        row_i as usize
    };
    let col1 = match col_i {
        Some(c) if c < 1 => {
            clamped = true;
            1
        }
        Some(c) => c as usize,
        None => 1,
    };
    Some(GotoTarget {
        row1,
        col1,
        clamped,
    })
}

/// 内联提示文案（卡片内提示区显示）：空输入 → 占位由输入框 placeholder 承担、提示区
/// 留空；无法解析 → 「不是有效行号」；合法 → 目标提示（clamp 时加「已钳制」前缀）。
/// 返回 `(文案, 是否错误色)`。
pub fn hint_for(input: &str, target: Option<&GotoTarget>) -> (String, bool) {
    if input.trim().is_empty() {
        return (String::new(), false);
    }
    match target {
        None => ("不是有效行号".to_string(), true),
        Some(t) => {
            let msg = if t.clamped {
                format!("已钳制到第 {} 行 第 {} 列", t.row1, t.col1)
            } else {
                format!("→ 第 {} 行 第 {} 列", t.row1, t.col1)
            };
            (msg, false)
        }
    }
}

/// 编辑区跳转行状态机（per-`CodeEditor` 一份；CodeEditor 本身 per-tab → goto 状态随 tab
/// 实体独立释放，切换/关闭 tab 无残留卡片/输入缓存）。
#[derive(Default)]
pub struct GoToOverlay {
    /// goto 卡是否打开。
    pub open: bool,
    /// 输入缓存（与输入框文本同步；Esc 关闭后保留，供再次 Ctrl+G 回显 + 重跳）。
    pub cached: String,
    /// 最近一次成功跳转的目标（1 基；`None` = 尚未成功跳转过/上次无效）。
    pub last: Option<(usize, usize)>,
    /// 防抖任务代号（过期任务落地被丢弃，陈旧结果不覆盖新输入）。
    pub flush_epoch: u64,
    /// 待 flush 标记（输入变化/Enter 置位，统一 flush 路径消费）。
    pub dirty: bool,
    /// 内联提示文案（空 = 不显示；解析成功前由输入实时刷新，跳转时随落点确认）。
    pub hint: String,
    /// 提示是否错误态（非法输入用错误色）。
    pub hint_error: bool,
}

impl GoToOverlay {
    /// 打开 goto 卡：置打开 + 排一次 flush（reopen 回显输入后由 view 调度防抖重跳）。
    pub fn open_bar(&mut self) {
        self.open = true;
        self.dirty = true;
    }

    /// 关闭 goto 卡：清除提示与待跳转标记，但**保留输入缓存**供再次打开回显。
    pub fn close_bar(&mut self) {
        self.open = false;
        self.dirty = false;
        self.hint.clear();
        self.hint_error = false;
    }

    /// 输入变化 / Enter 置脏：起新代号（旧防抖到期不再落地）。
    pub fn invalidate(&mut self) {
        self.dirty = true;
        self.flush_epoch = self.flush_epoch.wrapping_add(1);
    }

    /// 内联提示访问器：`(文案, 是否错误色)`（测试探针用）。
    pub fn hint(&self) -> (&str, bool) {
        (&self.hint, self.hint_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules() {
        assert_eq!(parse_goto("42"), Some((42, None)));
        assert_eq!(parse_goto("42:7"), Some((42, Some(7))));
        assert_eq!(parse_goto(" 42 : 7 "), Some((42, Some(7))));
        assert_eq!(parse_goto("0"), Some((0, None)));
        assert_eq!(parse_goto("-3"), Some((-3, None)));
        assert_eq!(parse_goto("abc"), None);
        assert_eq!(parse_goto("42:7:9"), None, "多个冒号应非法");
        assert_eq!(parse_goto("42:"), None, "空列应非法");
        assert_eq!(parse_goto(""), None, "空字符串应非法");
        assert_eq!(parse_goto(":"), None);
        assert_eq!(parse_goto("42:x"), None);
        assert_eq!(parse_goto("x:3"), None);
    }

    #[test]
    fn clamp_rules() {
        // 行 clamp：0/负 → 1；超行数 → 末行；正常保持；列 < 1 → 1；未给列按 1。
        assert_eq!(
            parse_and_clamp("42", 60),
            Some(GotoTarget { row1: 42, col1: 1, clamped: false })
        );
        assert_eq!(
            parse_and_clamp("0", 60),
            Some(GotoTarget { row1: 1, col1: 1, clamped: true })
        );
        assert_eq!(
            parse_and_clamp("-5", 60),
            Some(GotoTarget { row1: 1, col1: 1, clamped: true })
        );
        assert_eq!(
            parse_and_clamp("9999", 60),
            Some(GotoTarget { row1: 60, col1: 1, clamped: true })
        );
        assert_eq!(
            parse_and_clamp("42:0", 60),
            Some(GotoTarget { row1: 42, col1: 1, clamped: true })
        );
        assert_eq!(
            parse_and_clamp("42:7", 60),
            Some(GotoTarget { row1: 42, col1: 7, clamped: false })
        );
        // 非法 / 空 → None（不崩）。
        assert_eq!(parse_and_clamp("abc", 60), None);
        assert_eq!(parse_and_clamp("   ", 60), None);
        assert_eq!(parse_and_clamp("42:7:9", 60), None);
        // 空行数兜底（line_count=0 防御 → 1）。
        assert_eq!(
            parse_and_clamp("5", 0),
            Some(GotoTarget { row1: 1, col1: 1, clamped: true })
        );
    }

    #[test]
    fn hint_texts() {
        // 空输入 → 空提示（占位由输入框承担）。
        assert_eq!(hint_for("", None), (String::new(), false));
        assert_eq!(hint_for("  ", None), (String::new(), false));
        // 非法 → 错误文案 + 错误色。
        let (h, e) = hint_for("abc", None);
        assert_eq!((h.as_str(), e), ("不是有效行号", true));
        // 合法未钳制 → 目标文案。
        let (h, e) = hint_for("42", Some(&GotoTarget { row1: 42, col1: 1, clamped: false }));
        assert_eq!((h.as_str(), e), ("→ 第 42 行 第 1 列", false));
        // 钳制 → 已钳制文案。
        let (h, e) = hint_for("9999", Some(&GotoTarget { row1: 60, col1: 1, clamped: true }));
        assert_eq!((h.as_str(), e), ("已钳制到第 60 行 第 1 列", false));
    }

    #[test]
    fn overlay_open_close_keeps_cache_clears_hint() {
        let mut o = GoToOverlay::default();
        o.open_bar();
        assert!(o.open && o.dirty);
        o.cached = "42".to_string();
        o.hint = "→ 第 42 行 第 1 列".to_string();
        o.close_bar();
        assert!(!o.open && !o.dirty);
        assert_eq!(o.cached, "42", "输入缓存保留供再次打开回显");
        assert_eq!(o.hint, "", "关闭清除提示");
        // invalidate 递增代号（旧防抖作废）。
        let e0 = o.flush_epoch;
        o.invalidate();
        assert!(o.flush_epoch != e0 && o.dirty);
    }
}
