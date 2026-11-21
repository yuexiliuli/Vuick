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
use editor_core::buffer::LineWidthHistogram;
use memchr::memchr2;

use crate::encoding::{detect_source_encoding, is_truncated_utf8_tail};
use encoding_rs::GB18030;

/// 流式解码一块字节：`encoding_rs` 解码器跨块持有残缺多字节（`last` 仅末块为 true）。
/// 返回解码出的文本（空则 `None`）；遇到 malformed（GB18030/UTF-16 拒收，含二进制与
/// 截断多字节）→ `InvalidData`（保「二进制/截断 → 打开失败」既有语义）。
fn decode_stream(
    dec: &mut encoding_rs::Decoder,
    bytes: &[u8],
    is_last: bool,
) -> io::Result<Option<String>> {
    // 预分配足够容量（UTF-16 解码输出 ≤ 2 倍字节量），避免局中 OutputFull 扩容循环。
    let mut dst = String::with_capacity(bytes.len() * 2 + 16);
    let mut consumed = 0usize;
    loop {
        let (result, read) = dec
            .decode_to_string_without_replacement(&bytes[consumed..], &mut dst, is_last);
        consumed += read;
        match result {
            encoding_rs::DecoderResult::InputEmpty => break,
            encoding_rs::DecoderResult::OutputFull => {
                // `decode_to_string_without_replacement` **不扩容**：它以 String 当前容量
                // 为输出上限（encoding_rs 文档），缓存已满则原地返回 OutputFull 且不消费。
                // 必须主动补足容量再续解，否则同一容量下反复 OutputFull 死循环（挂死）。
                dst.reserve(bytes.len().saturating_sub(consumed) * 2 + 32);
            }
            encoding_rs::DecoderResult::Malformed(..) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not match detected encoding",
                ))
            }
        }
    }
    Ok(if dst.is_empty() { None } else { Some(dst) })
}

/// 首块级「编码探针」**首判点** = 首个分块目标字节数：读满首块量字节（或到文件尾）才做
/// 首判，随后进入流式解码——第一个分块本就要攒到该量才发出，探针不额外延迟首屏。
/// 首判点若「歧义」（本块全合法 UTF-8 但文件未结束 / 块尾截断多字节）→ 扩窗续读，
/// 见 [`ChunkReader::probe_is_ambiguous`] 与 [`PROBE_MAX_BYTES`]。
const PROBE_CEIL_BYTES: usize = OPEN_FIRST_CHUNK_BYTES;

/// 探针**硬上限**（扩窗上限）= 次块目标字节数（1MB）：首判点歧义时扩窗读到「首个坏字节」
/// 定编码，但**至多**读到该量——防把 >256KB 的纯 UTF-8 大文件整读到文件尾（内存与读盘
/// 都有界：probe 至多 1MB）。达上限仍全合法 → 按纯 UTF-8 处理（流式解码继续严格校验）。
const PROBE_MAX_BYTES: usize = OPEN_CHUNK_TARGET_BYTES;

/// 一块已归一化（LF）文本增量：`text` 只含完整行（除文件末尾可能不含换行的最后一行
/// 外，均以 `\n` 结尾），`hist` / `max_line_chars` 是这些完整行的精确行宽统计。
pub struct TextChunk {
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

/// 分块读盘器：一次取一块（[`Self::next_chunk`]），内部跨块维护编码残留、
/// `\r` 归一化、行不劈开等状态。
///
/// **编码感知**：打开解码顺序（用户确认）——① BOM 优先（UTF-8/UTF-16 LE/BE，去 BOM）；
/// ② 合法 UTF-8 前缀 → UTF-8 原样（严格校验沿用 `utf8_carry`）；③ 否则 GB18030
/// （覆盖 GBK/GB2312）。非 UTF-8 用 `encoding_rs` 单个 `Decoder` 跨块持有残缺多字节
/// 输入（Streaming 解码，不物化整篇）；解码遇 malformed（GB18030 对非法字节序列
/// 拒收，已探针验证）→ `InvalidData`（保「二进制/截断 → 打开失败」语义）。
pub struct ChunkReader {
    file: File,
    buf: Vec<u8>,
    /// 纯 UTF-8 路径的跨块 UTF-8 残留（≤3 字节不完整尾部）。
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
    /// 决定的源编码（[`crate::encoding`]）：`None` = 纯 UTF-8（无 BOM）；
    /// `Some(UTF_8)` = 带 BOM 的 UTF-8；`Some(UTF_16LE/BE)` / `Some(GB18030)` 同理。
    /// 决定前为 `None`（此时 `decided=false`），消费端 EOF 后经 [`Self::source_encoding`]
    /// 读取并写回选项卡。
    src_enc: Option<&'static encoding_rs::Encoding>,
    /// 编码决定前的累积字节（≤ [`PROBE_CEIL_BYTES`] 或至文件尾；决定后清空灌入）。
    probe: Vec<u8>,
    /// 编码是否已决定（进入流式解码阶段）。
    decided: bool,
    /// 非 UTF-8（含带 BOM UTF-8/UTF-16/GB18030）的流式解码器；`None` = 纯 UTF-8 严格路径。
    decoder: Option<encoding_rs::Decoder>,
}

impl ChunkReader {
    /// 用生产读缓冲（[`READ_BUF_BYTES`]）打开。
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        Self::open_with_buf(path, READ_BUF_BYTES)
    }

    /// 指定每次 `File::read` 的字节缓冲大小（测试可传小值以强制多块/边界分裂）。
    pub fn open_with_buf(path: &std::path::Path, read_buf: usize) -> io::Result<Self> {
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
            src_enc: None,
            probe: Vec::with_capacity(PROBE_CEIL_BYTES.min(64 * 1024)),
            decided: false,
            decoder: None,
        })
    }

    /// 决定出的源文本编码（EOF 后稳定）：`None` = 纯 UTF-8。消费端据此写回选项卡
    /// `source_encoding`（保存按此编码回写）。
    pub fn source_encoding(&self) -> Option<&'static encoding_rs::Encoding> {
        self.src_enc
    }

    /// 取下一个待发块。内部推进读盘直到产出一个块或到达文件尾；返回 `None` = 读尽
    /// （此时 [`Self::total_bytes`] 为总原始字节数）。
    pub fn next_chunk(&mut self) -> io::Result<Option<TextChunk>> {
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
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// 推进一个读缓冲：读到数据 → **编码决定**（首块级探针）或按已定编码流式解码 →
    /// 归一化 + 分块产出（入 `pending`）；读到 EOF → 收尾（孤立尾 `\r`、剩余完整行、
    /// 无尾换行的最后一行；未决多字节尾部 → InvalidData）。
    fn advance(&mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        let n = self.file.read(&mut self.buf)?;
        if n > 0 {
            self.total += n as u64;
            // 借位隔离：先拷出本块字节（探针累积 / 流式解码均只读它），避免
            // `&mut self` 方法调用与 `&self.buf` 的不可变借冲突。
            let bytes = self.buf[..n].to_vec();
            if !self.decided {
                // 编码决定前：累积探针（首块级），攒够 / EOF / 可定才决定（不做逐块喂、
                // 不把可能被 GB18030 重新解码的前缀先喂进 rope）。首块量已满但「歧义」
                // （见 probe_is_ambiguous：本块全合法 UTF-8 但文件未结束 / 块尾截断
                // 多字节）→ 扩窗续读至首个坏字节（或 PROBE_MAX_BYTES）再定——窗口边界处
                // 的跨块多字节序列不被误切块截断导致错判（GB18030 双字节与 UTF-8 共有
                // 头字节 0xC6 被劈开、260KB ASCII 前缀后接 GB18030 两类均由此捕获）。
                self.probe.extend_from_slice(&bytes);
                if self.probe.len() >= PROBE_CEIL_BYTES && !self.probe_is_ambiguous() {
                    self.finish_decide()?;
                }
                return Ok(());
            }
            self.feed_read(&bytes)?;
            return Ok(());
        }
        // EOF：若探针尚未决定编码 → 以全文决定；随后按流式解码器/严格 UTF-8 收尾。
        self.done = true;
        if !self.decided {
            self.finish_decide()?;
        }
        if let Some(dec) = self.decoder.as_mut() {
            // 非 UTF-8 编码器收尾（last=true）：处理流内未决的多字节尾部；截断（无法
            // 收尾）→ 报 InvalidData（与 `read_to_string` 的「截断多字节=失败」一致）。
            if let Some(tail) = decode_stream(dec, &[], true)? {
                feed(
                    &mut self.pending_cr,
                    &mut self.s,
                    &tail,
                    &mut self.first,
                    &mut self.pending,
                );
            }
        } else if !self.utf8_carry.is_empty() {
            // 纯 UTF-8 路径：文件在非法/截断多字节处结束 → 报 InvalidData（现状语义）。
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

    /// 探针当前是否「不足以定编码」、需扩窗续读：
    /// - 已见**确定坏字节**（`from_utf8` 报 `error_len==Some`）或 BOM → **可定**（false，
    ///   按 GB18030 / BOM 直定）；
    /// - 本块（或扩窗后）仍是**合法 UTF-8**（`Ok`）或**合法前缀 + 截断尾**（`error_len==None`），
    ///   且文件未结束 → **歧义**（true）：块内全合法不代表后续块没有 GB18030 字节（如
    ///   260KB ASCII 前缀后接 GB18030 4 字节），块尾截断也可能是 GB18030 双字节与 UTF-8
    ///   共有的头字节（0xC6，见 `probe_boundary_split_gb18030_char_not_misdetected`）。
    ///
    /// 达到 [`PROBE_MAX_BYTES`] 硬上限后歧义也按「可定」处理——纯 UTF-8 大文件占多数，
    /// 但限定读盘/内存上界 1MB（不整读、不无限扩）。
    fn probe_is_ambiguous(&self) -> bool {
        if self.probe.len() >= PROBE_MAX_BYTES {
            return false;
        }
        // BOM 已见即一锤定音（doc 契约「…或 BOM → 可定」）：UTF-16 BOM（FF FE / FE FF）
        // 本因 0xFF/0xFE 非法 UTF-8 走 `error_len==Some` 归为可定；此处补 UTF-8 BOM
        // （EF BB BF）——否则带 UTF-8 BOM 的纯合法内容文件被当歧义一直扩窗到 1MB/EOF
        // 才定，≥256KB 带 BOM 文件首屏延迟无谓扩大（扩窗不会带来不同编码结论）。
        if self.probe.starts_with(&[0xEF, 0xBB, 0xBF]) {
            return false;
        }
        match std::str::from_utf8(&self.probe) {
            Ok(_) => true,
            Err(e) => e.error_len().is_none(),
        }
    }

    /// 探针就绪（攒够首块量或文件尾）→ 按 [`detect_source_encoding`] 决定编码并把探针
    /// 整体灌入（BOM 已去 / 纯 UTF-8 直接喂 / 非 UTF-8 建解码器流式转）。决定后进入
    /// 流式解码阶段（首块产出时点不变：都要攒满首块量）。
    fn finish_decide(&mut self) -> io::Result<()> {
        let probe = std::mem::take(&mut self.probe);
        let (enc, bom) = detect_source_encoding(&probe);
        self.decided = true;
        // 探针恰在任意字节截止：若判「非 UTF-8 → GB18030」只因「合法 UTF-8 前缀 + 被块
        // 边界切开的残缺多字节尾」（error_len==None），那是**纯 UTF-8 的跨块切分**（常态，
        // 首块边界劈开中文三字节仅一例）——喂合法前缀、残尾进 `utf8_carry` 由下一块续解；
        // 否则整篇会被误判成 GB18030（乱码）。EOF 收尾时这里不再有下一块，残尾由
        // `advance` 的「utf8_carry 非空 → InvalidData」兜底（截断=失败，现状语义）。
        if enc == Some(GB18030) && bom == 0 && is_truncated_utf8_tail(&probe) {
            self.src_enc = None;
            let valid = std::str::from_utf8(&probe)
                .map(|s| s.len())
                .unwrap_or_else(|e| e.valid_up_to());
            if valid > 0 {
                feed(
                    &mut self.pending_cr,
                    &mut self.s,
                    std::str::from_utf8(&probe[..valid]).expect("前缀应合法 UTF-8"),
                    &mut self.first,
                    &mut self.pending,
                );
            }
            self.utf8_carry.extend_from_slice(&probe[valid..]);
            return Ok(());
        }
        self.src_enc = enc;
        let raw = &probe[bom..];
        match enc {
            None => {
                // 纯 UTF-8：探针整体已判定合法 → 直接喂（严格校验继续交给 `utf8_carry`。
                let s = std::str::from_utf8(raw).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
                })?;
                feed(
                    &mut self.pending_cr,
                    &mut self.s,
                    s,
                    &mut self.first,
                    &mut self.pending,
                );
            }
            Some(e) => {
                let mut dec = e.new_decoder_without_bom_handling();
                if !raw.is_empty()
                    && let Some(s) = decode_stream(&mut dec, raw, false)? {
                        feed(
                            &mut self.pending_cr,
                            &mut self.s,
                            &s,
                            &mut self.first,
                            &mut self.pending,
                        );
                    }
                self.decoder = Some(dec);
            }
        }
        Ok(())
    }

    /// 已定编码后的逐块解码：非 UTF-8 → `encoding_rs` 解码器（跨块持有残缺多字节）；
    /// 纯 UTF-8 → 沿用 `utf8_carry` + `error_len` 检测（现状路径不变）。
    fn feed_read(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(dec) = self.decoder.as_mut() {
            if let Some(s) = decode_stream(dec, bytes, false)? {
                feed(
                    &mut self.pending_cr,
                    &mut self.s,
                    &s,
                    &mut self.first,
                    &mut self.pending,
                );
            }
            Ok(())
        } else {
            // 合并 UTF-8 残留，取最长合法前缀（≤3 字节不完整尾部留到下一块）。
            // 与 fs::read_to_string 语义一致：整篇必须合法 UTF-8。error_len() == Some
            // （确定无效字节，而非「读缓冲截断的多字节尾部」）→ 立即报 InvalidData。
            self.utf8_carry.extend_from_slice(bytes);
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
            Ok(())
        }
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
pub struct ChunkBuilder {
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
pub fn produce_open_chunks(
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
    use editor_core::buffer::{compute_line_stats, LineWidthHistogram};
    use ropey::Rope;

    /// 诊断（`#[ignore]`，仅本机，真线程墙钟）：生产端「首块产出」的真实延迟——用
    /// 真实的 `大文档测试126MB.rs`（329 万行）直接走 `ChunkReader`（与 `read_chunks_async`
    /// 同一读法），量：文件打开、首个 256KB 块产出、以及在此之上整篇读完的花费。
    /// 「首块产出」即挂载可显示的前置耗时——若远大于几十 ms，需查磁盘/读缓冲配置。
    /// 跑法：`cargo test -p text_open open_progress::bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_real_first_chunk_wall_clock() {
        let path = std::env::var("BENCH_OPEN_FILE").unwrap_or_else(|_| {
            r"E:\Rust\gpui-editor\target\大文档测试126MB.rs".to_string()
        });
        let t_open = std::time::Instant::now();
        let mut reader = match ChunkReader::open(std::path::Path::new(&path)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SKIP: missing {path} ({e})");
                return;
            }
        };
        let t_first = std::time::Instant::now();
        let mut n = 0usize;
        let mut total_bytes = 0usize;
        let mut first_bytes = 0usize;
        loop {
            match reader.next_chunk().unwrap() {
                Some(c) => {
                    if n == 0 {
                        first_bytes = c.text.len();
                        eprintln!(
                            "[BENCH] 文件打开: {:.1} ms；首块(256KB)产出: {:.1} ms（{} B / {} 行）",
                            (t_first - t_open).as_secs_f64() * 1000.0,
                            t_first.elapsed().as_secs_f64() * 1000.0,
                            c.text.len(),
                            c.hist.values().sum::<usize>(),
                        );
                    }
                    n += 1;
                    total_bytes += c.text.len();
                }
                None => break,
            }
        }
        eprintln!(
            "[BENCH] 整篇读完: {:.1} ms（{} 块 / {} B / {} 行）",
            t_first.elapsed().as_secs_f64() * 1000.0,
            n,
            total_bytes,
            reader.total_bytes(),
        );
        let _ = first_bytes;
    }

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
    /// 运行：`cargo test --release -p text_open bench_progressive_producer_real -- \
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

    /// 复现 RED（B6，docs/code-review-2026-11-17/08-text_open.md H）：
    /// 探针式编码探测**只读首块**（`PROBE_CEIL_BYTES` = 256KB，open_progress.rs:69,186-193,254）。
    /// 构造 >256KB 的 GB18030 大文件：**首块（256KB 探针窗口）全是合法 UTF-8（ASCII 行）**、
    /// GB18030 4 字节字符落在窗口之后的后续块 → 现行判纯 UTF-8（None）→ 后续块走严格 UTF-8
    /// 路径遇坏字节 InvalidData **打开失败**；而 ≤128KB 同步路径 `decode_bytes` 全文件扫描同一
    /// 文件能开（shell_core/file_ops.rs:64-68）——异步/同步按文件大小分叉。
    /// 期望：探测读到首个坏字节并回溯/扩窗 → 判 GB18030 → 正常打开（本测试当前应为 RED）。
    #[test]
    fn large_gb18030_with_utf8_probe_prefix_opens_with_gb18030() {
        // 全 ASCII 行填充 260KB（> 探针窗口 256KB，窗口内必为合法 UTF-8），末尾接一个真实
        // GB18030 4 字节字符→整文件真实编码 = GB18030。
        let template = "abcdefghijklmnopqrstuvwxyz0123456789\n"; // 全 ASCII 行模板
        let mut filler = String::with_capacity(260 * 1024);
        while filler.len() < 260 * 1024 {
            filler.push_str(template);
        }
        filler.truncate(260 * 1024); // ASCII 区严格 260KB（truncate 在 ASCII 上按字节安全）
        let expected = format!("{filler}😀"); // 期望解码文本（末尾为 GB18030 4 字节字符）
        let file_bytes = GB18030.encode(&expected).0.into_owned();
        // 前置：整文件真实编码确为 GB18030（排除构造错误——此处若红不是目标缺陷）。
        assert_eq!(
            detect_source_encoding(&file_bytes),
            (Some(GB18030), 0),
            "整文件真实编码应为 GB18030"
        );
        with_temp_bytes(&file_bytes, |p| {
            // 生产路径 ChunkReader 直接驱动（128KB 读缓冲，与生产同量级）。
            let mut reader = ChunkReader::open_with_buf(p, 128 * 1024).unwrap();
            let mut text = String::new();
            while let Some(c) = reader.next_chunk().unwrap() {
                text.push_str(&c.text);
            }
            assert_eq!(
                reader.source_encoding(),
                Some(GB18030),
                "长 GB18030 文件应判 GB18030（当前只读首块→判 UTF-8→打开失败）"
            );
            assert_eq!(text, expected, "GB18030 全文应正确解码还原");
        });
    }

    /// 复现 RED（B6，同 H 的第二触发）：GB18030 双字节字符「七」（U+4E03，GB18030 字节
    /// `0xC6 0xDF`）恰被探针窗口边界劈开——首字节 `0xC6` 正好是合法 UTF-8 二字节首字节、落到
    /// 窗口尾部 → 窗口像「截断的 UTF-8 尾」→ 现行 `finish_decide` 走 `is_truncated_utf8_tail`
    /// (open_progress.rs:261-276) 误判纯 UTF-8 + carry → 后续块把 `0xDF` 并进 carry →
    /// `[0xC6,0xDF]` 非法 → InvalidData 打开失败。期望：跨块多字节序列不被误切块截断导致
    /// 错判 → 判 GB18030 正常打开（本测试当前应为 RED）。
    #[test]
    fn probe_boundary_split_gb18030_char_not_misdetected() {
        // ASCII 填充到「探针窗口 - 1」字节，让「七」的首字节恰落在窗口 (probe) 末尾。
        let template = "abcdefghijklmnopqrstuvwxyz0123456789\n";
        let mut filler = String::new();
        while filler.len() < PROBE_CEIL_BYTES - 1 {
            filler.push_str(template);
        }
        filler.truncate(PROBE_CEIL_BYTES - 1);
        let expected = format!("{filler}七");
        let file_bytes = GB18030.encode(&expected).0.into_owned();
        // 前置：「七」编码 2 字节、文件恰跨窗口边界，整文件真实编码确为 GB18030。
        assert_eq!(
            file_bytes.len(),
            PROBE_CEIL_BYTES + 1,
            "「七」应编码 2 字节、恰跨探针窗口"
        );
        assert_eq!(
            detect_source_encoding(&file_bytes),
            (Some(GB18030), 0),
            "整文件真实编码应为 GB18030"
        );
        with_temp_bytes(&file_bytes, |p| {
            // 逐字节读缓冲(1) → 探针绝不越窗吞掉 0xDF，跨块截断稳定复现。
            let mut reader = ChunkReader::open_with_buf(p, 1).unwrap();
            let mut text = String::new();
            while let Some(c) = reader.next_chunk().unwrap() {
                text.push_str(&c.text);
            }
            assert_eq!(
                reader.source_encoding(),
                Some(GB18030),
                "跨块截断的 GB18030 双字节应判 GB18030（当前误判 UTF-8→打开失败）"
            );
            assert_eq!(text, expected, "跨块 GB18030 双字节应完整解码还原");
        });
    }

    /// 绿测：普通 GB18030 文本（坏字节在首块窗口内、无需扩窗）经 ChunkReader 端到端应判
    /// GB18030 并正确解码——锁「窗口内即见坏字节 → 直定 GB18030」的快路径不回归。
    #[test]
    fn gb18030_with_early_bad_byte_decides_gb18030() {
        let expected = format!("普通中文文件内容第{}行：你好世界", 42);
        let file_bytes = GB18030.encode(&expected).0.into_owned();
        assert_eq!(
            detect_source_encoding(&file_bytes),
            (Some(GB18030), 0),
            "前置：整文件真实编码应为 GB18030"
        );
        with_temp_bytes(&file_bytes, |p| {
            let mut reader = ChunkReader::open_with_buf(p, 128 * 1024).unwrap();
            let mut text = String::new();
            while let Some(c) = reader.next_chunk().unwrap() {
                text.push_str(&c.text);
            }
            assert_eq!(
                reader.source_encoding(),
                Some(GB18030),
                "窗口内即见坏字节 → 应直判 GB18030（不扩窗、不误判 UTF-8）"
            );
            assert_eq!(text, expected, "GB18030 文件应完整解码还原");
        });
    }

    /// 绿测：> 1MB 的纯 UTF-8 文件（含多字节中文/emoji）走「扩窗至硬上限 → 判纯 UTF-8 →
    /// 流式续解」路径——锁「扩窗不能误伤长 UTF-8 大文件」：编码判定仍为纯 UTF-8、且跨
    /// 256KB/1MB 边界劈开的 3/4 字节序列在扩窗与后续流式解码中仍完整还原（回归守卫：
    /// B6 扩窗只补歧义判定，不以整读或改判为代价）。
    #[test]
    fn large_pure_utf8_file_stays_utf8_with_probe_expansion() {
        let template = "中文宽行 abcdefghijklmnopqrstuvwxyz 0123456789 😀\n";
        let mut expected = String::new();
        while expected.len() < 1_100 * 1024 {
            expected.push_str(template);
        }
        with_temp(&expected, |p| {
            let mut reader = ChunkReader::open_with_buf(p, 128 * 1024).unwrap();
            let mut text = String::new();
            while let Some(c) = reader.next_chunk().unwrap() {
                text.push_str(&c.text);
            }
            assert_eq!(
                reader.source_encoding(),
                None,
                ">1MB 纯 UTF-8 文件应判纯 UTF-8（扩窗至上限不误改判）"
            );
            assert_eq!(text, expected, "长 UTF-8 文件应完整还原（边界劈开多字节不损失）");
        });
    }

    /// 绿测（评审补，B6 扩窗批）：带 UTF-8 BOM（EF BB BF）的纯 UTF-8 大文本经 `ChunkReader`
    /// 端到端应判 `Some(UTF_8)`（去 BOM）并完整还原正文——锁「BOM 一锤定音」快路径：BOM 已见
    /// 即直定，不因文内全合法 UTF-8 被当歧义扩窗到 1MB/EOF（≥256KB 带 BOM 文件首屏延迟回归
    /// 守卫；`probe_is_ambiguous` 的 doc 契约「或 BOM → 可定」应被实现兑现）。
    #[test]
    fn utf8_bom_large_file_decides_utf8_without_probe_expansion() {
        let template = "带 BOM 的中文内容行 abcdefghijklmnopqrstuvwxyz 0123456789 😀\n";
        let mut body = String::new();
        while body.len() < 300 * 1024 {
            body.push_str(template);
        }
        let mut file_bytes = vec![0xEF, 0xBB, 0xBF];
        file_bytes.extend_from_slice(body.as_bytes());
        assert_eq!(
            detect_source_encoding(&file_bytes),
            (Some(encoding_rs::UTF_8), 3),
            "前置：带 BOM 文件应识别为 UTF-8(BOM)"
        );
        with_temp_bytes(&file_bytes, |p| {
            let mut reader = ChunkReader::open_with_buf(p, 7).unwrap();
            let mut text = String::new();
            while let Some(c) = reader.next_chunk().unwrap() {
                text.push_str(&c.text);
            }
            assert_eq!(
                reader.source_encoding(),
                Some(encoding_rs::UTF_8),
                "带 BOM 大文本应判 UTF-8(BOM)，不因纯合法内容被扩窗/改判"
            );
            assert_eq!(text, body, "去 BOM 后应完整还原正文（多字节跨块不损失）");
        });
    }
}
