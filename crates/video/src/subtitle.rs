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
/// 线性扫描（字幕按 `start_pts` 排序、窗口可重叠——相同 `start_pts` 或互相包裹的窗口
/// 使「二分按 start 找首个命中」不可靠，线性是唯一稳妥口径，与 ffmpeg 侧
/// `vff_subtitle_at` 同款）。取首个命中窗口。千级字幕 × 每拍 render ≈ 微秒级，非热点；
/// 且仅在**有字幕且当前有活动字幕**时才逐字解析文本，无事件时段零分配。
pub fn find_active_subtitle<'a>(
    subtitles: &'a [SubtitleEntry],
    seconds: f64,
) -> Option<&'a SubtitleEntry> {
    // 线性查找（字幕数量通常不多）
    subtitles.iter().find(|s| s.is_active_at(seconds))
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
