//! 字幕渲染模块。

use gpui::Hsla;

/// 一条字幕条目（与 decode 侧共用同一类型）。
pub use crate::decode::SubtitleEntry;

/// 字幕样式。
#[derive(Clone, Copy, Debug)]
pub struct SubtitleStyle {
    /// 字体大小（px）。
    pub font_size: f32,
    /// 主色。
    pub primary_color: Hsla,
    /// 描边色。
    pub outline_color: Hsla,
    /// 位置。
    pub position: SubtitlePosition,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            font_size: 24.0,
            primary_color: Hsla::white(),
            outline_color: Hsla::black(),
            position: SubtitlePosition::BottomCenter,
        }
    }
}

/// 字幕位置。
#[derive(Clone, Copy, Debug)]
pub enum SubtitlePosition {
    /// 底部居中（默认）。
    BottomCenter,
    /// 自定义位置（相对于视频区域左下角）。
    Custom { x: f32, y: f32 },
}

impl SubtitleEntry {
    /// 判断当前时间是否在字幕有效期内。
    pub fn is_active_at(&self, seconds: f64) -> bool {
        seconds >= self.start_pts && seconds <= self.end_pts
    }
}

/// 从 ASS 样式标记解析纯文本：
/// 1. `{\...}` 覆盖块（`\pos`/`\b1`/`\i1`/`\fad` 等）整体剔除；
/// 2. `<font ...>`/`<b>`/`<i>` 与 `</...>` 类 HTML 覆盖标记整体剔除（如
///    `<font face="微软雅黑" size="32"><b>…</b></font>`）；
/// 3. ASS 换行转义 `\N`/`\n` → 真换行（多行字幕）。
pub fn parse_ass_text(text: &str) -> String {
    let mut result = String::new();
    let mut in_override = false;
    let mut in_html = false;

    for ch in text.chars() {
        match ch {
            '{' => in_override = true,
            '}' => in_override = false,
            '<' => in_html = true,
            '>' => in_html = false,
            _ if !in_override && !in_html => result.push(ch),
            _ => {}
        }
    }

    // `\N`（硬换行）/`\n`（软换行）→ 真换行；两次 replace 顺序无所谓（结果不含反斜杠 n）
    result.replace("\\N", "\n").replace("\\n", "\n")
}

/// 查找当前时间应显示的字幕。
///
/// 字幕按 `start_pts` 升序（[`SubtitleReader::open`](`crate::video::decode`) 已 sort）：
/// **二分**定位最后一个 `start_pts ≤ seconds` 的条目，命中即返回 → 常规非重叠字幕
/// O(log N)。窗口可重叠（相同 `start_pts` 或互相包裹）时 end 无序无法按 start 剪枝，
/// 从该处**往前回扫**找命中——结果与线性同（返回一条活跃窗口）；回扫上限 = 全部，
/// 511 条量级仍 ≈ 微秒，每拍 render 只在「有活动字幕」时走一次，非热点。
pub fn find_active_subtitle<'a>(
    subtitles: &'a [SubtitleEntry],
    seconds: f64,
) -> Option<&'a SubtitleEntry> {
    let i = match subtitles.partition_point(|s| s.start_pts <= seconds) {
        0 => return None,
        n => n - 1,
    };
    // 从最近的（start 最大）往前找：常规情况只有 i（或 i 自身结束后的极少数重叠邻居）
    // 会出现，命中即回；未命中时回扫更早条目取「活跃」的那条。
    for e in subtitles[..=i].iter().rev() {
        if e.is_active_at(seconds) {
            return Some(e);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖块 + HTML 覆盖标记 + 换行转义应被清理成纯文本。
    #[test]
    fn parse_ass_text_strips_overrides_and_html_tags() {
        assert_eq!(parse_ass_text("{\\b1}Hello{\\b0}"), "Hello");
        assert_eq!(
            parse_ass_text(r#"<font color="&H0000FF&">测试</font>"#),
            "测试"
        );
        assert_eq!(parse_ass_text("<b>粗</b>体"), "粗体");
        assert_eq!(parse_ass_text("上\\N下"), "上\n下");
        assert_eq!(parse_ass_text("左\\n右"), "左\n右");
        // 目标来源（真实 MKV 常见样式）：
        let cleaned = parse_ass_text(
            r#"{\an8}<font face="微软雅黑" size="32"><b>这是一条测试字幕</b></font>"#,
        );
        assert!(cleaned.contains("这是一条测试字幕"));
        assert!(!cleaned.contains("<font") && !cleaned.contains("<b"));
        assert!(!cleaned.contains('{') && !cleaned.contains('}'));
    }

    /// 对拍「清洗前渲染 vs 清洗后渲染」：旧 render 每帧调 `parse_ass_text(raw)`；新实现把
    /// 清洗移到**加载时**（`entry.text = parse_ass_text(raw)` 存回，render 直接读 `entry.text`）。
    /// 两者逐字节一致 ⟺ `parse_ass_text` 幂等（双洗同输出）——覆盖真实 ASS 常见样式：
    /// 覆盖块 / HTML 标记 / 换行转义。
    #[test]
    fn parse_ass_text_move_to_load_keeps_render_output_identical() {
        let raws = [
            r#"{\an8}<font face="微软雅黑" size="32"><b>这是一条测试字幕</b></font>"#,
            r#"{\b1}Hello{\b0} World"#,
            "上\\N下 左\\n右",
            "<i>斜体</i>与{\\pos(0,0)}纯文本",
            "无样式纯文本",
        ];
        for raw in raws {
            let once = parse_ass_text(raw);
            assert_eq!(
                parse_ass_text(&once),
                once,
                "双洗同输出（旧=parse(raw)、新=读取已清洗文本）: {raw:?}"
            );
        }
    }

    /// 活动字幕按 [start, end] 闭区间命中；区间外不命中。
    #[test]
    fn find_active_subtitle_selects_by_window() {
        let subs = vec![
            SubtitleEntry {
                start_pts: 0.0,
                end_pts: 2.0,
                text: "a".into(),
            },
            SubtitleEntry {
                start_pts: 5.0,
                end_pts: 6.0,
                text: "b".into(),
            },
        ];
        assert_eq!(find_active_subtitle(&subs, 1.0).map(|s| s.text.as_str()), Some("a"));
        assert_eq!(find_active_subtitle(&subs, 2.0).map(|s| s.text.as_str()), Some("a"));
        assert_eq!(find_active_subtitle(&subs, 5.5).map(|s| s.text.as_str()), Some("b"));
        assert!(find_active_subtitle(&subs, 4.0).is_none());
        assert!(find_active_subtitle(&[], 1.0).is_none());
    }

    /// 重叠窗口（同一时间两条都活跃）二分取「最近开始」那条——显示最新对白；
    /// 常规非重叠场景与线性结果一致。
    #[test]
    fn find_active_subtitle_overlap_prefers_latest_start() {
        let ov = vec![
            SubtitleEntry {
                start_pts: 0.0,
                end_pts: 3.0,
                text: "early".into(),
            },
            SubtitleEntry {
                start_pts: 2.0,
                end_pts: 4.0,
                text: "late".into(),
            },
        ];
        assert_eq!(find_active_subtitle(&ov, 2.5).map(|s| s.text.as_str()), Some("late"));
        assert_eq!(find_active_subtitle(&ov, 3.5).map(|s| s.text.as_str()), Some("late"));
        assert!(find_active_subtitle(&ov, 4.5).is_none());
    }

    /// 对拍代表性样例集（与 `media-ffmpeg` crate `decode/tests.rs` 的
    /// `active_subtitle_index_edge_case_battery_cross_check` **逐字同源**，两侧同一份数据、
    /// 同一组采样点断言同一索引）：覆盖 0 时刻起点、相邻紧贴（前条 end == 后条 start）、
    /// 重叠跨段（前者窗口包裹后者起点）、窗口间隙、尾部越界、空集。
    #[test]
    fn find_active_subtitle_edge_case_battery() {
        let subs = vec![
            SubtitleEntry { start_pts: 0.0, end_pts: 1.0, text: "at-zero".into() }, // 0 时刻起点
            SubtitleEntry { start_pts: 1.0, end_pts: 2.0, text: "touching".into() }, // 紧贴 [0,1]|[1,2]
            SubtitleEntry { start_pts: 1.5, end_pts: 3.0, text: "overlap".into() }, // 跨段包裹 touching 起点
            SubtitleEntry { start_pts: 5.0, end_pts: 6.0, text: "tail".into() },
        ];
        let cases: &[(f64, Option<usize>)] = &[
            (-1.0, None), // 全部之前
            (0.0, Some(0)), // 0 时刻闭区间命中
            (0.5, Some(0)),
            (1.0, Some(1)), // 紧贴边界：两端活跃，取「最近开始」（touching）
            (1.75, Some(2)), // 重叠：overlap 起于 1.5，覆盖 touching → 取最近开始
            (2.5, Some(2)), // overlap 独立活跃
            (3.0, Some(2)), // 含结束端点（闭区间）
            (3.5, None), // 窗口间隙
            (5.0, Some(3)),
            (6.0, Some(3)), // 含结束端点（闭区间）
            (6.5, None), // 尾部越界
            (99.0, None),
        ];
        assert!(
            find_active_subtitle(&[], 0.0).is_none(),
            "空集不命中"
        );
        for &(t, want) in cases {
            let got = find_active_subtitle(&subs, t)
                .map(|e| subs.iter().position(|s| std::ptr::eq(s, e)).unwrap());
            assert_eq!(got, want, "t={t}: find_active_subtitle 应选中索引 {want:?}");
        }
    }

    /// `is_active_at` 含端点（结束秒处仍显示，与实际播放时钟一致）。
    #[test]
    fn entry_is_active_at_uses_inclusive_bounds() {
        let e = SubtitleEntry {
            start_pts: 20.05,
            end_pts: 22.03,
            text: "t".into(),
        };
        assert!(e.is_active_at(20.05));
        assert!(e.is_active_at(22.03));
        assert!(!e.is_active_at(22.04));
        assert!(!e.is_active_at(19.9));
    }
}
