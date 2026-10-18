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

use config::limits::{
    OPEN_CHUNK_TARGET_BYTES, OPEN_FIRST_CHUNK_BYTES, READ_BUF_BYTES,
};
use editor::buffer::LineWidthHistogram;
use memchr::memchr2;

/// 一块已归一化（LF）文本增量：`text` 只含完整行（除文件末尾可能不含换行的最后一行
/// 外，均以 `\n` 结尾），`hist` / `max_line_chars` 是这些完整行的精确行宽统计。
pub(crate) struct TextChunk {
    /// 归一化文本增量。
    pub text: String,
    /// 本块完整行的宽度直方图（宽度单位 → 行数，见 [`LineWidthHistogram`]）。
    pub hist: LineWidthHistogram,
    /// 本块完整行的最长行字符数（Unicode 标量）。
    pub max_line_chars: usize,
    /// 本块文本的精确字符数（Unicode 标量）：与 `text.chars().count()` 同值，但由
    /// **字节级统计路径** O(1) 累积（`finish_line` 把该行 `suffix_chars` 并入），
    /// 不在消费端/后台另做整篇 `chars()` 重扫——大文档状态栏「数字跳动」的累计
    /// 字符进度用它（行数进度 = `hist.values().sum()`，块内隐式携带）。
    pub chars: usize,
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
            // 与 fs::read_to_string 语义一致：整篇必须合法 UTF-8。此前只取合法前缀、
            // 非法尾段则静默吞掉，导致大二进制（exe/dll）打开成只剩少量乱码前缀的页、
            // 且永不弹「打开失败」。现改为：error_len() == Some（确定无效字节，
            // 而非「读缓冲截断的多字节尾部」）→ 立即报 InvalidData。
            self.utf8_carry.extend_from_slice(&self.buf[..n]);
            let parsed = std::str::from_utf8(&self.utf8_carry);
            if let Err(e) = &parsed
                && e.error_len().is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                ));
            }
            let valid_len = match parsed {
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
        // EOF：收尾。若仍残留字节（文件在非法/截断多字节处结束），与 read_to_string
        // 语义一致报 InvalidData，不再静默丢弃。
        self.done = true;
        if !self.utf8_carry.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            ));
        }
        if self.pending_cr {
            self.pending_cr = false;
            self.s.finish_line();
        }
        if !self.s.out.is_empty() {
            self.pending.push_back(TextChunk {
                text: std::mem::take(&mut self.s.out),
                hist: std::mem::take(&mut self.s.out_hist),
                max_line_chars: std::mem::take(&mut self.s.out_mx_chars),
                chars: std::mem::take(&mut self.s.out_chars),
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
                chars: self.s.suffix_chars,
            });
        }
        Ok(())
    }
}

/// 统计一段（ASCII 分隔符之间的）行内容的宽度单位与字符数——**字节级快路径**：
/// ASCII 单步、多字节按首字节长度整跳（输入是已校验的合法 UTF-8）。与旧逐字符
/// `char_width_units`（Tab=4 / ASCII=1 / 其它=2）完全同值，debug 下约一个数量级更快
/// （大文档生产端主要耗时点：128MB 逐 `chars()` 迭代 ≈ 8.5s → 本路径 ≈ 1s 内）。
#[inline]
fn line_byte_stats(bytes: &[u8]) -> (usize, usize) {
    let mut units = 0usize;
    let mut chars = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c < 0x80 {
            units += if c == b'\t' { 4 } else { 1 };
            chars += 1;
            i += 1;
        } else {
            units += 2;
            chars += 1;
            i += if c >= 0xF0 {
                4
            } else if c >= 0xE0 {
                3
            } else {
                2
            };
        }
    }
    (units, chars)
}

/// 块大小达到目标即整块发出（`TextChunk` 只含完整行、均以 `\n` 结尾——非末块绝不劈行）。
/// 与旧实现的「每条完整行落定后检查」语义一致（`out` 只在 `finish_line` 时增长）。
fn maybe_ship(s: &mut ChunkBuilder, first: &mut bool, pending: &mut VecDeque<TextChunk>) {
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
            chars: std::mem::take(&mut s.out_chars),
        });
        *first = false;
    }
}

/// 把一批（UTF-8 合法、可跨 `\r` 状态的）文本灌入分块器；产出的块追加进 `pending`。
///
/// **字节级快路径**（替代旧版逐字符 `chars()` 的 5–10×）：`memchr2` 找 `\r`/`\n` 分隔符，
/// 行内容整段 `push_str` + [`line_byte_stats`] 一次计宽计字数。跨块状态（`\r` 残留、
/// 未结束行 `suffix`）与旧实现逐位一致——换行归一化/行不劈块/直方图语义零变化。
fn feed(
    pending_cr: &mut bool,
    s: &mut ChunkBuilder,
    text: &str,
    first: &mut bool,
    pending: &mut VecDeque<TextChunk>,
) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if *pending_cr {
            *pending_cr = false;
            if bytes[i] == b'\n' {
                // CRLF：\r 由上一轮置位，\n 结束当前行（suffix 里是该行内容）。
                s.finish_line();
                i += 1;
                maybe_ship(s, first, pending);
                continue;
            }
            // 孤立 `\r`：先结束当前行，再照常处理当前字节。
            s.finish_line();
        }
        match bytes[i] {
            b'\r' => {
                *pending_cr = true;
                i += 1;
            }
            b'\n' => {
                s.finish_line();
                i += 1;
                maybe_ship(s, first, pending);
            }
            _ => {
                // 行内容：扫到下一个 `\r`/`\n`（或块尾），整段处理一次。
                let start = i;
                let rest = &bytes[i..];
                let end = i + memchr2(b'\n', b'\r', rest).unwrap_or(rest.len());
                let part = &text[start..end];
                let (units, chars) = line_byte_stats(part.as_bytes());
                s.suffix.push_str(part);
                s.suffix_units += units;
                s.suffix_chars += chars;
                i = end;
            }
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
    /// `out` 中这些行的精确字符数（Unicode 标量；随 [`Self::finish_line`] 累积，
    /// 供 [`TextChunk::chars`] 状态栏进度用，O(1) 不重扫）。
    out_chars: usize,
    /// 当前**未结束**行（不随块发出：换行绝不跨块劈开）。
    suffix: String,
    suffix_units: usize,
    suffix_chars: usize,
}

impl ChunkBuilder {
    /// 结束当前行（`\n` 终止）：把行内容并入 `out`、宽度纳入直方图、最长行求 max。
    /// 空行（`"a\n\n"` 的中间行）也计一个宽度 0 条目，与
    /// [`editor::buffer::compute_line_stats`] 对非「尾幻影空行」的行一致。
    fn finish_line(&mut self) {
        self.out_mx_chars = self.out_mx_chars.max(self.suffix_chars);
        *self.out_hist.entry(self.suffix_units).or_insert(0) += 1;
        self.out.push_str(&self.suffix);
        self.out.push('\n');
        // 字符数含行终止 `\n`（+1）：整块 `out_chars` 才与 `out.chars().count()` 精确同值，
        // 也才能与换装后的 `Rope::len_chars()`（全文含换行）口径一致——状态栏「数字跳动」
        // 的最后一步不会因「进度只算行内容、终值含换行」而出现 +N 行的末跳（评审 WARN-2）。
        self.out_chars += self.suffix_chars + 1;
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
    use editor::buffer::{compute_line_stats, LineWidthHistogram};
    use ropey::Rope;

    /// 诊断（`#[ignore]`，需显式跑）：消费端逐块追加（`rope.insert(末尾)`）在主线程的真实
    /// 成本——只测带统计合并前最核心的 rope 追加（P1 前实测 128MB/312万行 debug 下每 1MB
    /// ~35ms、累计 4.5s → 主线程冻结元凶；P1 已改为后台一次建 rope + EOF O(1) 换装）。
    #[test]
    #[ignore]
    fn temp_consumer_append_cost() {
        let line = "x".repeat(79);
        let mut chunk = String::with_capacity(1_048_600);
        while chunk.len() < 1_048_576 {
            chunk.push_str(&line);
            chunk.push('\n');
        }
        let mut rope = ropey::Rope::new();
        let t = std::time::Instant::now();
        let mut ms_total = 0.0;
        for i in 0..128 {
            let t1 = std::time::Instant::now();
            rope.insert(rope.len(), &chunk);
            ms_total += t1.elapsed().as_secs_f64() * 1000.0;
            if i % 16 == 15 {
                eprintln!(
                    "  累计 {}{} B / {:.1} ms（本批 {:.1} ms）",
                    (i + 1) * 1_048_576 / 1024 / 1024,
                    "MB",
                    ms_total,
                    t1.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        eprintln!(
            "128×1MB rope末尾insert 累计: {:.1} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// 基准：生产端 `ChunkReader` 真实全流程（真实线程/磁盘：读→UTF-8 校验→CRLF 归一→
    /// 逐块行宽统计）的**纯 CPU 墙钟**——不经过 fake 调度器的通道背压停摆，用于对比
    /// 「读盘+归一+统计」合计与原始字节数（debug ≈ release × 10，看相对变化）。
    /// 运行：`cargo test --release -p shell bench_progressive_producer_real -- \
    ///        --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_progressive_producer_real() {
        let path = std::env::var("BENCH_OPEN_FILE").unwrap_or_else(|_| {
            r"E:\Rust\gpui-editor\target\大文档测试126MB.txt".to_string()
        });
        if !std::path::Path::new(&path).is_file() {
            eprintln!("SKIP: missing {path}");
            return;
        }
        let file_mb = std::fs::metadata(&path).map(|m| m.len() / 1024 / 1024).unwrap_or(0);
        let t0 = std::time::Instant::now();
        let mut reader = ChunkReader::open(std::path::Path::new(&path)).unwrap();
        let mut n = 0usize;
        let mut bytes = 0usize;
        while let Some(c) = reader.next_chunk().unwrap() {
            n += 1;
            bytes += c.text.len();
        }
        let secs = t0.elapsed().as_secs_f64();
        println!(
            "==== 生产端 ChunkReader 基准：{file_mb} MB / {n} 块 / {bytes} B 归一文本 ===="
        );
        println!("  真实线程全流程: {secs:.2} s（{:.0} MB/s 归一输出）", bytes as f64 / 1e6 / secs);
    }


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

    /// 进程内并发测试的唯一后缀：同一 pid 且落在同一 Windows 100ns 时钟 tick 内的两次
    /// `SystemTime::now()` 会返回相同 nanos → 文件名相同 → 后写覆盖前读（跨测试污染，
    /// 见 `chunks_reassemble_to_normalized_text` 曾读到别家内容）。用原子计数器保证每次
    /// 调用唯一；跨进程仍由 pid 区分。
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn with_temp(content: &str, f: impl FnOnce(&std::path::Path)) {
        let dir = std::env::temp_dir().join(format!(
            "gpui-progress-{}-{}-{}.txt",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
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

    fn with_temp_bytes(content: &[u8], f: impl FnOnce(&std::path::Path)) {
        let dir = std::env::temp_dir().join(format!(
            "gpui-progress-{}-{}-{}.bin",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&dir, content).unwrap();
        f(&dir);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn invalid_utf8_stream_reports_invalid_data() {
        // 大二进制（读分支即遇非法字节）：必须报 InvalidData，绝不静默吞尾喂出乱码页。
        with_temp_bytes(&[0xFF; 300_000], |p| {
            let err = produce_open_chunks(p, 128 * 1024, |_| Ok(())).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidData,
                "非法 UTF-8 流应报 InvalidData（与 read_to_string 一致）"
            );
        });
    }

    #[test]
    fn truncated_multibyte_at_eof_reports_invalid_data() {
        // 文件在多字节序列被截断处结束：合法前缀照常喂出，EOF 残留（utf8_carry 非空）
        // 必须报 InvalidData，不能静默丢弃。用极小读缓冲强制把尾字残留在 carry。
        with_temp_bytes(b"abc\xF0\x9F\x98", |p| {
            let err = produce_open_chunks(p, 8, |_| Ok(())).unwrap_err();
            assert_eq!(
                err.kind(),
                io::ErrorKind::InvalidData,
                "EOF 残留截断多字节应报 InvalidData"
            );
        });
    }

    #[test]
    fn valid_multibyte_utf8_reads_at_any_buf() {
        // 对照：合法多字节 UTF-8（含跨读块截断）在任意读缓冲下都不能误报，正常路径原样工作。
        let raw = "中文😀好数据中abc\n第二行🚀\n".repeat(1_000);
        with_temp(&raw, |p| {
            let (got, ..) = collect(p, 7); // 极小读缓冲 → 多字节跨块频繁截断
            assert_eq!(got, raw, "极小读缓冲下合法多字节应逐块完整拼接");
            let (got2, ..) = collect(p, 128 * 1024); // 生产量级读缓冲
            assert_eq!(got2, raw, "生产读缓冲下合法多字节应完整读出");
        });
    }
}
