//! Styled text construction and long-word wrapping.

use super::model::{MarkdownTheme, Span};
use gpui::{font, px, SharedString, StyledText, TextRun, UnderlineStyle};

fn is_unbroken_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '\u{00C0}'..='\u{024F}')
        || matches!(c, '\u{0400}'..='\u{04FF}')
        || matches!(
            c,
            '-' | '_' | '.' | '\'' | '$' | '%' | '@' | '#' | '^' | '~' | ',' | '=' | ':'
        )
}

const UNBROKEN_LIMIT: usize = 20;
const UNBROKEN_STEP: usize = 14;

/// 在不可断的超长连续片段中插入零宽空格，让长 URL 和长串能够换行。
pub fn break_long_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut run = 0usize;
    for ch in text.chars() {
        if is_unbroken_word_char(ch) {
            run += 1;
            out.push(ch);
            if run > UNBROKEN_LIMIT && run.is_multiple_of(UNBROKEN_STEP) {
                out.push('\u{200b}');
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

pub(super) fn spans_to_styled(
    spans: &[Span],
    theme: &MarkdownTheme,
    force_bold: bool,
) -> SpannedText {
    let base = font(".SystemUIFont");
    let mono = font(theme.mono_family.clone());
    let mut runs = Vec::new();
    let mut text = String::new();
    for sp in spans {
        let mut font = if sp.code { mono.clone() } else { base.clone() };
        if sp.bold || force_bold {
            font = font.bold();
        }
        if sp.italic {
            font = font.italic();
        }
        let segment = break_long_runs(&sp.text);
        runs.push(TextRun {
            len: segment.len(),
            font,
            color: if sp.code {
                theme.code_text
            } else if sp.link {
                theme.link
            } else {
                theme.text
            },
            background_color: sp.code.then_some(theme.code_bg),
            underline: sp.link.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(theme.link),
                wavy: false,
            }),
            strikethrough: sp.strike.then_some(gpui::StrikethroughStyle {
                thickness: px(1.),
                color: Some(theme.text),
            }),
        });
        text.push_str(&segment);
    }
    SpannedText {
        text: text.into(),
        runs,
    }
}

pub(super) struct SpannedText {
    pub(super) text: SharedString,
    pub(super) runs: Vec<TextRun>,
}

impl SpannedText {
    pub(super) fn into_element(self) -> StyledText {
        StyledText::new(self.text).with_runs(self.runs)
    }
}
