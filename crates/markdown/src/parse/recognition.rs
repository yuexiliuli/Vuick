// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Vuick contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Shared markdown-construct **recognition** — the definitions both of
//! Zorite's engines consume so they can never drift apart (links navigated in
//! the reader for months while WYSIWYG ignored clicks; alerts were once
//! recognized in three separate places). The reader (this crate's view),
//! the WYSIWYG editor (`vuick`), and any other consumer (PDF export)
//! share *what counts as a construct and what's its payload*; each keeps its
//! own rendering. Everything here is engine-neutral and gpui-free.
//!
//! 自 `markdown::live`（已移除）迁至 `markdown` 顶层，供 `wysiwyg`/`view` 共用。
//! 部分构造辅助（列表编号、正文方向、属性、内嵌、块锚点等）原仅供 zorite 编辑器
//! 使用、现暂空闲——作为共享识别 API 保留，待 `wysiwyg` 逐项接入后恢复。
// #![allow(dead_code)]

/// The five GitHub alert kinds (`> [!NOTE]` …).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

/// `(kind, marker text)` for each alert, in matching order.
pub const ALERT_MARKERS: [(AlertKind, &str); 5] = [
    (AlertKind::Note, "[!NOTE]"),
    (AlertKind::Tip, "[!TIP]"),
    (AlertKind::Important, "[!IMPORTANT]"),
    (AlertKind::Warning, "[!WARNING]"),
    (AlertKind::Caution, "[!CAUTION]"),
];

impl AlertKind {
    /// The title rendered in place of the marker ("Note", "Tip", …).
    pub fn label(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Tip => "Tip",
            Self::Important => "Important",
            Self::Warning => "Warning",
            Self::Caution => "Caution",
        }
    }
}

/// [`alert_marker`] for a single line's body (text after a blockquote's `>`
/// prefix): tolerates leading spaces and returns the kind, the byte length
/// consumed within `body` (spaces, marker, fold char, one separator space) —
/// what a line-oriented editor hides before painting the label — and the fold
/// state (`Some(true)` = folded).
pub fn alert_prefix(body: &str) -> Option<(AlertKind, usize, Option<bool>)> {
    let trimmed = body.trim_start();
    let ws = body.len() - trimmed.len();
    for (kind, m) in ALERT_MARKERS {
        if let Some(rest) = trimmed.strip_prefix(m) {
            let (fold, flen) = match rest.as_bytes().first() {
                Some(b'-') => (Some(true), 1),
                Some(b'+') => (Some(false), 1),
                _ => (None, 0),
            };
            let rest = &rest[flen..];
            if rest.is_empty() {
                return Some((kind, ws + m.len() + flen, fold));
            }
            if rest.starts_with(' ') {
                return Some((kind, ws + m.len() + flen + 1, fold));
            }
        }
    }
    None
}

/// The fold char of the alert marker on `line` (a full source line, `>` prefix
/// included): its byte offset within the line and the current state
/// (`true` = `-`/folded). `None` when the line isn't a foldable alert marker.
pub fn alert_fold_char(line: &str) -> Option<(usize, bool)> {
    let b = line.as_bytes();
    let mut p = 0;
    while p < b.len() && (b[p] == b'>' || b[p] == b' ') {
        p += 1;
    }
    let (_, _, fold) = alert_prefix(&line[p..])?;
    let folded = fold?;
    // The fold char sits right after the marker's closing `]`.
    let close = line[p..].find(']')? + p;
    Some((close + 1, folded))
}

/// What a click on a link-like construct targets. `Page` opens a page by
/// title (a `[[wiki-link]]` or a `#tag` — Logseq semantics); `Url` is an
/// inline or bare URL (hosts open http(s) externally, resolve files
/// themselves).
#[derive(Debug, PartialEq, Clone)]
pub enum LinkHit {
    Page(String),
    Url(String),
    /// A `((id))` block reference (Logseq-style frontend form) — the host
    /// resolves the id to its page + anchor.
    BlockRef(String),
}

/// Whether `url` may be handed to the OS URL opener (`cx.open_url` —
/// `NSWorkspace openURL:` on macOS, `ShellExecute` on Windows).
///
/// Link targets are *attacker-authorable*: a synced note, an imported vault, a
/// PDF's `/URI` annotation. The OS opener runs whichever handler owns the
/// scheme, so `smb://` leaks NTLM hashes on Windows, `file://` launches local
/// content, and app-registered schemes (`ms-msdt:` …) are reachable. Hence an
/// allowlist, never a denylist: only `http://`, `https://`, and `mailto:`
/// pass. Schemes are case-insensitive per RFC 3986, so `HTTP://` passes too.
///
/// `mailto:` is on the list because `[write us](mailto:x@y.com)` is ordinary
/// markdown and dropping it would break real notes. It opens a compose window
/// rather than running anything, and the mail client — not us — owns parsing
/// its query (a percent-encoded `%0D%0A` can't be neutralized here, and
/// clients have long restricted which headers a `mailto:` may set).
///
/// Whitespace and control characters anywhere are a rejection rather than
/// something to trim — openers strip them, so ` javascript:…` and
/// `java\tscript:…` would otherwise walk past a prefix check. Everything else
/// is rejected: `javascript:`, `data:`, `file:`, `smb:`, UNC
/// `\\server\share`, scheme-relative `//host/path`, and bare relative paths.
/// A host that wants to resolve local files does so itself, before the opener.
pub fn is_safe_external_url(url: &str) -> bool {
    !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && ["http://", "https://", "mailto:"].iter().any(|scheme| {
            url.as_bytes()
                .get(..scheme.len())
                .is_some_and(|got| got.eq_ignore_ascii_case(scheme.as_bytes()))
        })
}

/// Split a wiki-link's inner text into `(target, display)`:
/// `target|label` shows `label` (falling back to the target when the label is
/// empty); `name` shows itself. Both sides trimmed.
pub fn wiki_target_display(inner: &str) -> (&str, &str) {
    match inner.split_once('|') {
        Some((t, l)) if !l.trim().is_empty() => (t.trim(), l.trim()),
        Some((t, _)) => (t.trim(), t.trim()),
        None => (inner.trim(), inner.trim()),
    }
}

/// Whether `c` can appear inside a `#tag` name (after the `#`). `/` is
/// included — Logseq-style namespaced tags (`#area/sub`) are one tag.
pub fn is_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'/')
}

/// A word character for boundary checks (a `#` glued to a word isn't a tag;
/// a URL glued to a word isn't a link).
pub fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Where a bare URL starting at `start` ends: consumes to whitespace or a
/// wrapping delimiter, then backs off trailing punctuation (GFM-ish).
pub fn url_end(line: &str, start: usize) -> usize {
    let b = line.as_bytes();
    let mut j = start;
    while j < line.len()
        && !b[j].is_ascii_whitespace()
        && !matches!(b[j], b'<' | b'>' | b'"' | b'`')
    {
        j += 1;
    }
    while j > start
        && matches!(
            b[j - 1],
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b')' | b']'
        )
    {
        j -= 1;
    }
    j
}

/// O(1) 摊还的「下一个出现位置」游标：把 closer 的「每次从候选点全段重扫」换成
/// 「只前移的单调查找」。每个位置至多被同一游标扫一次、整段分摊 O(n)——消除
/// 未闭合定界符（无数 `[`/`((`/反引号对收盘）触发的 O(n²) DoS（P0-B1，2026-11-17）。
///
/// 前提：对**同一个** `pred` 的查询 `from` 必须单调不减（各扫描循环的主游标 `i`
/// 只增不减，天然满足；个别 footnote→inline 连续两段同型查询虽 from 回退一位，
/// 游标也只前移、返回「>=from 的最小命中」，与该回退位语义一致，见各调用处注释）。
pub(crate) struct Seek<F>
where
    F: Fn(usize) -> bool,
{
    len: usize,
    pos: usize,
    pred: F,
}

impl<F> Seek<F>
where
    F: Fn(usize) -> bool,
{
    pub(crate) fn new(len: usize, pred: F) -> Self {
        Self { len, pos: 0, pred }
    }

    /// 第一个 `>= from` 且满足 `pred` 的位置；没有则 `None`。
    /// 语义与旧 `find1`/`find2` 的「范围内第一个命中」完全一致。
    pub(crate) fn closer(&mut self, from: usize) -> Option<usize> {
        if from > self.pos {
            self.pos = from;
        }
        while self.pos < self.len && !(self.pred)(self.pos) {
            self.pos += 1;
        }
        (self.pos < self.len).then_some(self.pos)
    }
}

/// Every clickable link in `line`, as `(source byte range, target)`.
/// Wiki-links (anywhere on one opens its target; the alias is display-only),
/// inline `[text](url)` links, `#tags`, and bare `http(s)://` URLs. Images
/// (`![](src)`), footnote refs, and anything inside inline code are opaque —
/// not links. One grammar for every renderer's click hit-tests, hover
/// cursors, and styling.
pub fn links(line: &str) -> Vec<(std::ops::Range<usize>, LinkHit)> {
    let b = line.as_bytes();
    let end = line.len();
    let mut out = Vec::new();
    let mut i = 0;
    // P0-B1（2026-11-17）：未闭合定界符（无数 `[`/`((`/反引号对收盘）时旧实现每候选
    // 都对尾部整段 `find1`/`find2` 重扫、失败仅 `i += 1` → O(n²) DoS。closer 查找
    // 全部换成 O(1) 摊还的 [`Seek`] 游标（next-occurrence 记忆化），语义与旧 finds 一致。
    let mut nxt_bt = Seek::new(end, |p: usize| b[p] == b'`');
    let mut nxt_rb = Seek::new(end, |p: usize| b[p] == b']');
    let mut nxt_rb2 = Seek::new(
        end,
        |p: usize| p + 1 < end && b[p] == b']' && b[p + 1] == b']',
    );
    let mut nxt_rp = Seek::new(end, |p: usize| b[p] == b')');
    let mut nxt_rp2 = Seek::new(
        end,
        |p: usize| p + 1 < end && b[p] == b')' && b[p + 1] == b')',
    );
    while i < end {
        let c = b[i];
        // Inline code: the span is opaque (a URL inside backticks is verbatim).
        if c == b'`'
            && let Some(close) = nxt_bt.closer(i + 1)
        {
            i = close + 1;
            continue;
        }
        // Wiki-link: [[target]] / [[target|alias]].
        if c == b'['
            && i + 1 < end
            && b[i + 1] == b'['
            && let Some(close) = nxt_rb2.closer(i + 2)
        {
            let (target, _) = wiki_target_display(&line[i + 2..close]);
            if !target.is_empty() {
                out.push((i..close + 2, LinkHit::Page(target.to_string())));
            }
            i = close + 2;
            continue;
        }
        // Block ref: `((id))` — an anchor-shaped id (word chars / `-`).
        if c == b'('
            && i + 1 < end
            && b[i + 1] == b'('
            && let Some(close) = nxt_rp2.closer(i + 2)
        {
            let id = &line[i + 2..close];
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            {
                out.push((i..close + 2, LinkHit::BlockRef(id.to_string())));
                i = close + 2;
                continue;
            }
        }
        // Footnote reference [^label]: styled like a link but not one.
        if c == b'['
            && i + 1 < end
            && b[i + 1] == b'^'
            && let Some(rb) = nxt_rb.closer(i + 2)
            && rb > i + 2
        {
            i = rb + 1;
            continue;
        }
        // Inline link [text](url) — or an image ![alt](src), which is NOT a
        // link click (images render as widgets / have their own machinery).
        // 注：footnote 分支（b[i+1]=='^'）先查询过 nxt_rb（from=i+2）；此处 from=i+1
        // 回退一位，但游标只前移、必然返回「>=i+1 的第一个 `]`」且 b[i+1]=='^'≠`]`，
        // 与该情形原 find1 结果一致。
        if c == b'['
            && let Some(rb) = nxt_rb.closer(i + 1)
            && rb + 1 < end
            && b[rb + 1] == b'('
            && let Some(rp) = nxt_rp.closer(rb + 2)
        {
            let is_image = i > 0 && b[i - 1] == b'!';
            let url = line[rb + 2..rp].trim();
            if !is_image && !url.is_empty() {
                out.push((i..rp + 1, LinkHit::Url(url.to_string())));
            }
            i = rp + 1;
            continue;
        }
        // Tag: #tag → the page of that name (Logseq semantics).
        if c == b'#' && (i == 0 || !is_word_char(b[i - 1])) {
            let mut j = i + 1;
            while j < end && is_tag_char(b[j]) {
                j += 1;
            }
            if j > i + 1 {
                out.push((i..j, LinkHit::Page(line[i + 1..j].to_string())));
                i = j;
                continue;
            }
        }
        // Bare URL: http(s)://… at a word boundary (GFM autolink literal).
        // Compare BYTES: `i` walks bytes, so a str slice here would panic
        // mid-char on any non-ASCII text.
        if (b[i..].starts_with(b"http://") || b[i..].starts_with(b"https://"))
            && (i == 0 || !is_word_char(b[i - 1]))
        {
            let j = url_end(line, i);
            if j > i + 8 {
                out.push((i..j, LinkHit::Url(line[i..j].to_string())));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_reaches_the_os_opener() {
        for ok in [
            "http://example.com",
            "https://example.com/a?b=c#d",
            "HTTPS://EXAMPLE.COM",
            "HtTp://example.com",
            // Ordinary markdown — an email link opens a compose window.
            "mailto:a@b.c",
            "MAILTO:a@b.c",
        ] {
            assert!(is_safe_external_url(ok), "should allow {ok:?}");
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "file:///etc/passwd",
            "smb://evil/share",
            "vbscript:msgbox",
            "ms-msdt:/id",
            "//evil.com/path",
            r"\\evil\share",
            "/local/path",
            "example.com",
            " javascript:alert(1)",
            "\tjavascript:alert(1)",
            "java\tscript:alert(1)",
            "http\n://example.com",
            "https://exa\u{0}mple.com",
            "https://example.com\r\nHost: evil",
            "",
        ] {
            assert!(!is_safe_external_url(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn links_cover_every_kind() {
        let hits = links("see [[Page|alias]] and [x](https://a.io) #tag/sub https://b.io/p, done");
        assert_eq!(
            hits.iter().map(|(_, h)| h).collect::<Vec<_>>(),
            vec![
                &LinkHit::Page("Page".into()),
                &LinkHit::Url("https://a.io".into()),
                &LinkHit::Page("tag/sub".into()),
                &LinkHit::Url("https://b.io/p".into()), // trailing comma trimmed
            ]
        );
        // Opaque: code spans, images, footnotes, glued #.
        assert!(links("`https://x.io` ![a](i.png) [^1] word#no").is_empty());
        // Regression: multi-byte text before a URL must not panic the
        // byte-wise walk (it once str-sliced at a continuation byte).
        let hits = links("shrug ¯\\_(ツ)_/¯ then https://a.io done");
        assert_eq!(hits.len(), 1);
    }

    /// P0 红测（2026-11-17，修复前必须红）：`"[".repeat(n)`（n 个未闭合 `[`，无任何 `]`）
    /// 时，每个 `[` 位置都会先对尾部整段跑 `find2` 找 `]]`（wiki 分支）、再跑 `find1`
    /// 找 `]`（inline 分支），全部失败后才 `i += 1` → 修复前整体 O(n²) 重复全行重扫。
    /// n 拍到「修复前 debug 约 1–3 秒」（下面按实测调参记录证据）；1s 阈值取「明显小于
    /// 修复前耗时、明显大于修复后线性扫描预期（<1ms）」，红线只求「病态输入有界返回」。
    #[test]
    fn links_unclosed_brackets_bounded_time() {
        let n = 10_000;
        let line = "[".repeat(n);
        let t = std::time::Instant::now();
        let hits = links(&line);
        let ms = t.elapsed().as_millis();
        assert!(hits.is_empty());
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "links 对 {} 个未闭合 `[` 耗时 {ms}ms（O(n²) 病态，红测）",
            n
        );
    }
}
