//! 大文档「分块读盘 · 渐进装配」打开管线的前端（纯函数部分）。
//!
//! 目标（对齐 VS Code 大文件打开体感）：
//! 1. **不再一次性物化整篇 `String`**：逐块读盘（读多少、转多少、发多少），消除
//!    「读盘 String ≈ 文件大小」这份临时代理；
//! 2. **首屏尽早可见 + 随读随显**：每满一块（首块小、之后 1MB）就把这块归一化文本
//!    推进给 UI 线程追加进编辑器，内容渐进出现；
//! 3. **行宽统计随块精确补齐**（而非旧的整篇 `compute_line_stats` ≈7s / 抽样占位）：
//!    每块的「整行」宽度直方图在后台边读边算，UI 只需合并 → 横向滚动条量程渐进准确。
//!
//! 跨块边界正确性这里全部兜住：
//! - UTF-8 多字节序列被读盘截断 → 残留字节跨块携带；
//! - `\r\n` / `\r` 换行归一化到 `\n`（含 `\r` 恰好落在块尾、`\n` 落在下块开头）；
//! - 行绝不跨块劈开（整行语义保证逐行宽度统计与全盘一致）。
//!
//! 对外主入口是 [`ChunkReader`]（一次一块、内部推进读盘），应用层可配合**有界**异步
//! 通道（`send().await` 背压）逐块推给 UI——既不物化整篇 String，也不会让通道把整篇
//! 囤起来（背压保证最多若干块滞留在途）。测试用 [`produce_open_chunks`] 同步收尾。

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read};

use crate::config::limits::{
    OPEN_CHUNK_TARGET_BYTES, OPEN_FIRST_CHUNK_BYTES, READ_BUF_BYTES,
};
use crate::editor::buffer::{LineWidthHistogram, char_width_units};

/// 一块已归一化（LF）文本增量：`text` 只含完整行（除文件末尾可能不含换行的最后一行
/// 外，均以 `\n` 结尾），`hist` / `max_line_chars` 是这些完整行的精确行宽统计。
pub(crate) struct TextChunk {
    /// 归一化文本增量。
    pub text: String,
    /// 本块完整行的宽度直方图（宽度单位 → 行数，见 [`LineWidthHistogram`]）。
    pub hist: LineWidthHistogram,
    /// 本块完整行的最长行字符数（Unicode 标量）。
    pub max_line_chars: usize,
}

/// 分块读盘器：一次取一块（[`Self::next_chunk`]），内部跨块维护 UTF-8 残留、
/// `\r` 归一化、行不劈开等状态。
pub(crate) struct ChunkReader {
    file: File,
    buf: Vec<u8>,
    utf8_carry: Vec<u8>,
    pending_cr: bool,
    s: ChunkBuilder,
    /// 首块用更小的目标下限（更快出首屏）。
    first: bool,
    /// 已读完（EOF 收尾已产生/处理完 pending）。
    done: bool,
    /// 一次读盘可产出的多个块（`feed` 产出，逐块取走；FIFO）。
    pending: VecDeque<TextChunk>,
    /// 已读的原始字节总数。
    total: u64,
}

impl ChunkReader {
    /// 用生产读缓冲（[`READ_BUF_BYTES`]）打开。
    pub(crate) fn open(path: &std::path::Path) -> io::Result<Self> {
        Self::open_with_buf(path, READ_BUF_BYTES)
    }

    /// 指定每次 `File::read` 的字节缓冲大小（测试可传小值以强制多块/边界分裂）。
    pub(crate) fn open_with_buf(path: &std::path::Path, read_buf: usize) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            buf: vec![0u8; read_buf.max(1)],
            utf8_carry: Vec::with_capacity(4),
            pending_cr: false,
            s: ChunkBuilder::default(),
            first: true,
            done: false,
            pending: VecDeque::new(),
            total: 0,
        })
    }

    /// 取下一个待发块。内部推进读盘直到产出一个块或到达文件尾；返回 `None` = 读尽
    /// （此时 [`Self::total_bytes`] 为总原始字节数）。
    pub(crate) fn next_chunk(&mut self) -> io::Result<Option<TextChunk>> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Ok(Some(chunk));
            }
            if self.done {
                return Ok(None);
            }
            self.advance()?;
        }
    }

    /// 已读的原始字节总数（EOF 后稳定）。
    pub(crate) fn total_bytes(&self) -> u64 {
        self.total
    }

    /// 推进一个读缓冲：读到数据 → UTF-8 校验 + 归一化 + 分块产出（入 `pending`）；
    /// 读到 EOF → 收尾（孤立尾 `\r`、剩余完整行、无尾换行的最后一行）。
    fn advance(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        let n = self.file.read(&mut self.buf)?;
        if n > 0 {
            self.total += n as u64;
            // 合并 UTF-8 残留，取最长合法前缀（≤3 字节不完整尾部留到下一块）。
            self.utf8_carry.extend_from_slice(&self.buf[..n]);
            let valid_len = match std::str::from_utf8(&self.utf8_carry) {
                Ok(_) => self.utf8_carry.len(),
                Err(e) => e.valid_up_to(),
            };
            feed(
                &mut self.pending_cr,
                &mut self.s,
                std::str::from_utf8(&self.utf8_carry[..valid_len])
                    .expect("前缀应合法 UTF-8"),
                &mut self.first,
                &mut self.pending,
            );
            self.utf8_carry.drain(..valid_len);
            return Ok(());
        }
        // EOF：收尾。
        self.done = true;
        if self.pending_cr {
            self.pending_cr = false;
            self.s.finish_line();
        }
        if !self.s.out.is_empty() {
            self.pending.push_back(TextChunk {
                text: std::mem::take(&mut self.s.out),
                hist: std::mem::take(&mut self.s.out_hist),
                max_line_chars: std::mem::take(&mut self.s.out_mx_chars),
            });
        }
        if !self.s.suffix.is_empty() {
            let mut hist = LineWidthHistogram::new();
            if self.s.suffix_units > 0 {
                hist.insert(self.s.suffix_units, 1);
            }
            self.pending.push_back(TextChunk {
                text: std::mem::take(&mut self.s.suffix),
                hist,
                max_line_chars: self.s.suffix_chars,
            });
        }
        Ok(())
    }
}

/// 把一批（UTF-8 合法、可跨 `\r` 状态的）文本灌入分块器；产出的块追加进 `pending`。
fn feed(
    pending_cr: &mut bool,
    s: &mut ChunkBuilder,
    text: &str,
    first: &mut bool,
    pending: &mut VecDeque<TextChunk>,
) {
    for ch in text.chars() {
        if *pending_cr {
            *pending_cr = false;
            if ch != '\n' {
                // 孤立 `\r`：先结束当前行，再照常处理 ch。
                s.finish_line();
            }
            // ch == '\n'：CRLF → 归为一个换行（由下面 '\n' 分支结束当前行）。
        }
        match ch {
            '\r' => *pending_cr = true,
            '\n' => s.finish_line(),
            _ => {
                s.suffix.push(ch);
                s.suffix_units += char_width_units(ch);
                s.suffix_chars += 1;
            }
        }
        let target = if *first {
            OPEN_FIRST_CHUNK_BYTES
        } else {
            OPEN_CHUNK_TARGET_BYTES
        };
        if s.out.len() >= target {
            pending.push_back(TextChunk {
                text: std::mem::take(&mut s.out),
                hist: std::mem::take(&mut s.out_hist),
                max_line_chars: std::mem::take(&mut s.out_mx_chars),
            });
            *first = false;
        }
    }
}

/// 分块器的跨块状态。
#[derive(Default)]
struct ChunkBuilder {
    /// 已完成的完整行（待发块），均以 `\n` 结尾。
    out: String,
    /// `out` 中这些行的宽度直方图增量（自上一块发出以来）。
    out_hist: LineWidthHistogram,
    /// `out` 中这些行的最长行字符数。
    out_mx_chars: usize,
    /// 当前**未结束**行（不随块发出：换行绝不跨块劈开）。
    suffix: String,
    suffix_units: usize,
    suffix_chars: usize,
}

impl ChunkBuilder {
    /// 结束当前行（`\n` 终止）：把行内容并入 `out`、宽度纳入直方图、最长行求 max。
    /// 空行（`"a\n\n"` 的中间行）也计一个宽度 0 条目，与
    /// [`crate::editor::buffer::compute_line_stats`] 对非「尾幻影空行」的行一致。
    fn finish_line(&mut self) {
        self.out_mx_chars = self.out_mx_chars.max(self.suffix_chars);
        *self.out_hist.entry(self.suffix_units).or_insert(0) += 1;
        self.out.push_str(&self.suffix);
        self.out.push('\n');
        self.suffix.clear();
        self.suffix_units = 0;
        self.suffix_chars = 0;
    }
}

/// 同步读取一整篇，把每块交给 `on_chunk`；返回原始字节总数。供测试（生产路径用
/// [`ChunkReader`] 配合有界异步通道逐块外推）。
#[cfg(test)]
pub(crate) fn produce_open_chunks(
    path: &std::path::Path,
    read_buf: usize,
    mut on_chunk: impl FnMut(TextChunk) -> io::Result<()>,
) -> io::Result<u64> {
    let mut reader = ChunkReader::open_with_buf(path, read_buf)?;
    while let Some(chunk) = reader.next_chunk()? {
        on_chunk(chunk)?;
    }
    Ok(reader.total_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::{compute_line_stats, LineWidthHistogram};
    use gpui_component::input::Rope;

    fn collect(path: &std::path::Path, read_buf: usize) -> (String, LineWidthHistogram, usize) {
        let mut text = String::new();
        let mut hist = LineWidthHistogram::new();
        let mut mx_chars = 0usize;
        produce_open_chunks(path, read_buf, |chunk| {
            text.push_str(&chunk.text);
            for (w, c) in chunk.hist.iter() {
                *hist.entry(*w).or_insert(0) += c;
            }
            mx_chars = mx_chars.max(chunk.max_line_chars);
            Ok(())
        })
        .unwrap();
        (text, hist, mx_chars)
    }

    fn with_temp(content: &str, f: impl FnOnce(&std::path::Path)) {
        let dir = std::env::temp_dir().join(format!(
            "gpui-progress-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&dir, content).unwrap();
        f(&dir);
        let _ = std::fs::remove_file(&dir);
    }

    fn expected_hist(text: &str) -> (LineWidthHistogram, usize) {
        let (mx, hist) = compute_line_stats(&Rope::from(text));
        (hist, mx)
    }

    #[test]
    fn chunks_reassemble_to_normalized_text() {
        // 覆盖：CRLF、孤立 CR、无尾换行、空行、非 ASCII 多字节。
        let raw = "abc\r\ndef\rghi\n\n中文行内容\r\n\r最后无换行😀";
        with_temp(raw, |p| {
            let (got, ..) = collect(p, 16); // 极小读缓冲 → 多块 + 大量边界
            assert_eq!(
                got, "abc\ndef\nghi\n\n中文行内容\n\n最后无换行😀",
                "归一化应 CRLF/CR→LF"
            );
        });
    }

    #[test]
    fn normal_read_buf_reassembles() {
        let raw = (0..50_000)
            .map(|i| format!("第{i}行：测试 abcdefghijklmnopqrstuvwxyz 0123456789\r\n"))
            .collect::<String>();
        with_temp(&raw, |p| {
            let (got, ..) = collect(p, 256 * 1024);
            let expected = raw.replace("\r\n", "\n");
            assert_eq!(got, expected, "生产读缓冲下逐块拼接应等于归一化全文");
        });
    }

    #[test]
    fn chunk_boundaries_never_split_lines() {
        let raw = (0..20_000)
            .map(|i| format!("line {i:06} 中文宽度测试\n"))
            .collect::<String>();
        with_temp(&raw, |p| {
            let mut chunks = Vec::new();
            produce_open_chunks(p, 1, |chunk| {
                chunks.push(chunk);
                Ok(())
            })
            .unwrap();
            assert!(chunks.len() > 1, "极小读缓冲 + 短行应发出多块");
            let text = chunks.iter().map(|c| c.text.as_str()).collect::<String>();
            assert_eq!(text, raw, "逐块拼接应等于原文（全部块含行）");
            for (i, c) in chunks.iter().enumerate() {
                // 除末块外每块都要以 '\n' 结尾（整行语义：行绝不跨块）。
                if i + 1 < chunks.len() {
                    assert!(c.text.ends_with('\n'), "非末块 {i} 应以换行结尾");
                }
            }
        });
    }

    #[test]
    fn histogram_matches_full_scan_without_trailing_newline() {
        let raw = "aaa\naaaa\n中文行中文行\n\nbb\r\nccc";
        with_temp(raw, |p| {
            let (text, hist, mx_chars) = collect(p, 1);
            let expected_text = "aaa\naaaa\n中文行中文行\n\nbb\nccc";
            assert_eq!(text, expected_text);
            let (e_hist, e_mx) = expected_hist(expected_text);
            assert_eq!(
                hist, e_hist,
                "无尾换行：直方图应与全量 compute_line_stats 逐项一致"
            );
            assert_eq!(mx_chars, e_mx, "最长行字符数应一致");
        });
    }

    #[test]
    fn histogram_matches_full_scan_except_phantom_tail() {
        // 文件以 '\n' 结尾时，全量统计多一个「尾幻影空行」(宽度 0)——分块按 str::lines
        // 语义不产生它；行为上只影响直方图的宽度 0 计数，不影响最宽行/横向量程。
        let raw = "a\nbb\n中文行\n";
        with_temp(raw, |p| {
            let (text, hist, mx_chars) = collect(p, 3);
            assert_eq!(text, "a\nbb\n中文行\n");
            let (mut e_hist, e_mx) = expected_hist("a\nbb\n中文行\n");
            // 去掉全量的尾幻影空行（宽度 0）条目后应与分块直方图一致。
            if let Some(c) = e_hist.get_mut(&0) {
                *c -= 1;
                if *c == 0 {
                    e_hist.remove(&0);
                }
            }
            assert_eq!(hist, e_hist, "除尾幻影空行外应逐项一致");
            assert_eq!(mx_chars, e_mx);
        });
    }

    #[test]
    fn empty_file_yields_no_chunks() {
        with_temp("", |p| {
            let mut count = 0;
            let total = produce_open_chunks(p, 64, |_| {
                count += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(total, 0);
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn lone_cr_across_chunk_and_ship_boundary() {
        // `\r\n` 与孤立 `\r` 恰好跨读块/发行边界都被正确归一。
        with_temp("line1\r\nline2\rline3\nline4\r\n", |p| {
            let (text, ..) = collect(p, 5);
            assert_eq!(text, "line1\nline2\nline3\nline4\n");
        });
    }
}
