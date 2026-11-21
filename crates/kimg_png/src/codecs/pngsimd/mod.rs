//! 自研 PNG 解码引擎（SimdCodecs 移植第一炮）：块解析 → IDAT 有界流式解压 → 逐行反滤波 → EXPAND 展开。
//!
//! 输出语义与 png crate `Transformations::EXPAND` 逐字节一致（灰度<8 扩 8、调色板扩 RGB/RGBA、tRNS 扩 alpha、
//! 16 位保留大端），见 `expand.rs` 详注。非隔行「恒等展开」路径直接在目标缓冲内反滤波（零拷贝）；
//! Adam7 按遍展开后散射回全帧。
//!
//! 注释中文；不改动其它 crate；对外 API 由上层 `codecs::png::PngDecoder` 不变承载。

pub(crate) mod chunk;
pub(crate) mod expand;
pub(crate) mod pipeline;
pub(crate) mod predict;

use std::io::BufRead;

use crate::codecs::pngsimd::chunk::{ADAM7_PASSES, FileHeader, pass_extent};
use crate::codecs::pngsimd::expand::{ExpandKind, output_bpp};
use crate::codecs::pngsimd::predict::{
    packed_row_bytes, prediction_bpp, unfilter_row, unfilter_to,
};
use kimg_core::color::ColorType;
use kimg_core::error::{
    DecodingError, ImageError, ImageFormatHint, ImageResult, LimitError, LimitErrorKind,
};
use kimg_core::format::ImageFormat;

/// Adam7 scatter 快道开关：`KIMG_PNG_ADAM7_FAST=0|false|off` 强制走逐像素慢道（同二进制 A/B 裁决用）。
/// 默认开启；首次访问缓存一次环境变量，与 `kimg_core::simd` 的 env 闸同款惰性缓存姿势。
pub(crate) fn adam7_fast_enabled() -> bool {
    use std::sync::OnceLock;
    static FAST: OnceLock<bool> = OnceLock::new();
    *FAST.get_or_init(|| {
        std::env::var("KIMG_PNG_ADAM7_FAST")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off")))
            .unwrap_or(true)
    })
}

/// 分阶段计时探针（`stage_profile` feature）：decode 按 inflate / unfilter / expand / copy 累计耗时。
/// 正常构建（未开 feature）下结构体照常幸存但无计时指令，仅多一次 `&mut Stages` 透传——零开销。
#[derive(Default, Clone, Copy)]
#[cfg_attr(not(feature = "stage_profile"), allow(dead_code))]
pub(crate) struct Stages {
    /// decode 整体墙钟。
    total_ns: u64,
    /// IDAT → inflate 缓冲。
    inflate_ns: u64,
    /// 反滤波总耗时。
    unfilter_ns: u64,
    /// EXPAND 展开总耗时。
    expand_ns: u64,
    /// 逐行/逐像素拷贝（含 Adam7 scatter）。
    copy_ns: u64,
}

/// 阶段计时工具（仅 `stage_profile` feature 编译）。
#[cfg(feature = "stage_profile")]
mod stage_probe {
    use super::Stages;
    use std::time::Instant;

    #[inline]
    pub(super) fn ns(t0: Instant, t1: Instant) -> u64 {
        t1.duration_since(t0).as_nanos() as u64
    }

    /// 首次调用读取一次环境变量（进程级缓存）。
    fn enabled() -> bool {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            std::env::var("KIMG_PNG_STAGE_PROFILE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        })
    }

    /// 打印 `STAGE <label> pipeline=a|b inflate=..ms unfilter=..ms expand=..ms copy=..ms others=..ms wall=..ms inflated=..`
    /// 行（仅 env 开启时）。`pipeline` = 是否双核管道（B）。
    pub(super) fn report(st: &Stages, label: &str, inflated: usize, pipeline: bool) {
        if !enabled() {
            return;
        }
        let ms = |v: u64| v as f64 / 1e6;
        let others = st
            .total_ns
            .saturating_sub(st.inflate_ns + st.unfilter_ns + st.expand_ns + st.copy_ns);
        println!(
            "STAGE {label} pipeline={} inflate={:.3}ms unfilter={:.3}ms expand={:.3}ms copy={:.3}ms others={:.3}ms wall={:.3}ms inflated={inflated}",
            if pipeline { "b" } else { "a" },
            ms(st.inflate_ns),
            ms(st.unfilter_ns),
            ms(st.expand_ns),
            ms(st.copy_ns),
            ms(others),
            ms(st.total_ns),
        );
    }
}

/// PNG 引擎的输入载体（零拷贝 A1）：借用外部切片，或自有缓冲区。
///
/// `from_slice` 路径（`&[u8]` / `Cursor<&[u8]>` 的 `load_from_memory`）直接借用调用方输入，
/// 省掉一次整帧 `extend_from_slice` 复制（大图 MB 级输入可到亚毫秒量级，峰值内存同步下降）；
/// 读取器 / 文件路径仍走 `parse_with_capacity` 读入 `Owned`。两路在解析后字节语义完全一致。
#[derive(Clone)]
pub(crate) enum Input<'a> {
    /// 借用调用方切片（零拷贝；生命周期 ≤ 输入存活期）。
    Borrowed(&'a [u8]),
    /// 自有缓冲区（读取器/字符串构造路径）。
    Owned(Vec<u8>),
}

impl<'a> Input<'a> {
    /// 只读视图。
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Input::Borrowed(b) => b,
            Input::Owned(v) => v,
        }
    }
}

/// 解析好的 PNG 引擎：持有整份输入（供解码时按 IDAT 偏移流式喂给 inflate，避免重新拼接拷贝）与头信息。
///
/// 输入可为 `Input::Borrowed`（零拷贝借用，`'a` = 输入切片存活期）或 `Input::Owned`（自有，无借用，
/// 协变为任意 `'a`）。所有解码方法只读 `as_slice()`，与输入形态无关。
pub(crate) struct PngEngine<'a> {
    data: Input<'a>,
    header: FileHeader,
}

fn err_decode(msg: impl Into<String>) -> ImageError {
    ImageError::Decoding(DecodingError::new(
        ImageFormatHint::Exact(ImageFormat::Png),
        msg.into(),
    ))
}

fn err_limits() -> ImageError {
    ImageError::Limits(LimitError::from_kind(LimitErrorKind::InsufficientMemory))
}

impl<'a> PngEngine<'a> {
    /// 从读取器读入全部字节并解析（仅测试用；生产走 [`Self::parse_with_capacity`]）。
    #[cfg(test)]
    pub(crate) fn parse(r: &mut impl BufRead) -> ImageResult<PngEngine<'static>> {
        Self::parse_with_capacity(r, 0)
    }

    /// 同 [`Self::parse`]，但可按读取器剩余长度预留容量。读入用 `BufRead::fill_buf` +
    /// `consume` 循环：Cursor<&[u8]> / `&[u8]` 的 `fill_buf` **零拷贝返回整段剩余**，一次
    /// `extend_from_slice` 完成（单次 memcpy）；`read_to_end` 会走几十次 4KB 小缓冲 `read`
    /// 调用 + Vec 扩容拷贝（实测 8.3MB 输入 2-3ms → 单次 memcpy ~0.2-0.5ms）。`capacity_hint`
    /// 仅是预留给（不进逻辑/不出错），0 表示未知走扩容路径。
    /// 返回自有输入引擎（`'static`，可协变到任意 `'a`）。
    pub(crate) fn parse_with_capacity(
        r: &mut impl BufRead,
        capacity_hint: usize,
    ) -> ImageResult<PngEngine<'static>> {
        let mut data = Vec::new();
        if capacity_hint > 0 {
            data.try_reserve_exact(capacity_hint)
                .map_err(|_| err_limits())?;
        }
        loop {
            let buf = r.fill_buf().map_err(ImageError::IoError)?;
            if buf.is_empty() {
                break;
            }
            let n = buf.len();
            data.extend_from_slice(buf);
            r.consume(n);
        }
        let header = chunk::parse_png(&data)?;
        Ok(PngEngine {
            data: Input::Owned(data),
            header,
        })
    }

    /// 零拷贝构造：直接借用调用方切片解析（不再复制整份输入）。
    pub(crate) fn from_slice(bytes: &'a [u8]) -> ImageResult<PngEngine<'a>> {
        let header = chunk::parse_png(bytes)?;
        Ok(PngEngine {
            data: Input::Borrowed(bytes),
            header,
        })
    }

    /// 自有缓冲构造（调用方已持有完整 PNG 字节）。
    pub(crate) fn from_vec(data: Vec<u8>) -> ImageResult<PngEngine<'static>> {
        let header = chunk::parse_png(&data)?;
        Ok(PngEngine {
            data: Input::Owned(data),
            header,
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.header.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.header.height
    }

    /// EXPAND 输出色彩类型（= 现 kimg png crate 路径 `output_color_type` 的结果）。
    pub(crate) fn color_type(&self) -> ColorType {
        output_color_type(&self.header)
    }

    /// 原始 eXIf 块。
    pub(crate) fn exif(&self) -> Option<Vec<u8>> {
        self.header.exif.clone()
    }

    /// 输出缓冲总字节。
    pub(crate) fn total_bytes(&self) -> u64 {
        u64::from(self.header.width)
            * u64::from(self.header.height)
            * output_bpp(&self.header) as u64
    }

    /// 解码整帧到 `buf`（长度需等于 `total_bytes`）。字节级 = png crate EXPAND 输出。
    pub(crate) fn decode(&self, buf: &mut [u8], max_alloc: Option<u64>) -> ImageResult<()> {
        debug_assert_eq!(buf.len() as u64, self.total_bytes());
        let h = &self.header;
        let mut st = Stages::default();
        #[cfg(feature = "stage_profile")]
        let t0 = std::time::Instant::now();

        let data = self.data.as_slice();
        // 期望解压尺寸（防炸弹预检已保证不溢出到分配）。
        let expected = usize::try_from(chunk::inflated_size(
            h.width,
            h.height,
            h.bit_depth,
            h.color_type,
            h.interlace == 1,
        ))
        .unwrap_or(usize::MAX);
        let compressed: usize = h.idat_chunks.iter().map(|&(_, l)| l).sum();
        let pipelined = pipeline::pipeline_enabled(h, compressed, expected);

        let rows = if pipelined {
            // 双核管道（仅非隔行；内部自起生产者线程）。
            pipeline::decode_scanlines_pipelined(h, data, buf, expected, max_alloc, &mut st)
        } else {
            #[cfg(feature = "stage_profile")]
            let t1 = std::time::Instant::now();
            let inflated = inflate_idat(h, data, max_alloc)?;
            #[cfg(feature = "stage_profile")]
            let t2 = std::time::Instant::now();
            let r = if h.interlace == 1 {
                decode_adam7(h, &inflated, buf, &mut st)
            } else {
                decode_scanlines(h, &inflated, buf, &mut st)
            };
            #[cfg(feature = "stage_profile")]
            {
                st.inflate_ns = stage_probe::ns(t1, t2);
            }
            r
        };
        match rows {
            Ok(()) => {
                #[cfg(feature = "stage_profile")]
                {
                    st.total_ns = stage_probe::ns(t0, std::time::Instant::now());
                    stage_probe::report(&st, &self.stage_label(), expected, pipelined);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// STAGE 行场景标签（`宽x高 色彩类型`）。
    #[cfg(feature = "stage_profile")]
    fn stage_label(&self) -> String {
        let ct = match output_color_type(&self.header) {
            ColorType::L8 => "L8",
            ColorType::L16 => "L16",
            ColorType::La8 => "La8",
            ColorType::La16 => "La16",
            ColorType::Rgb8 => "Rgb8",
            ColorType::Rgb16 => "Rgb16",
            ColorType::Rgba8 => "Rgba8",
            ColorType::Rgba16 => "Rgba16",
            _ => "?",
        };
        format!("{}x{} {}", self.header.width, self.header.height, ct)
    }

    /// 仅诊断：分阶段计时。
    #[cfg(test)]
    fn decode_timed(&self, buf: &mut [u8]) -> ImageResult<()> {
        let h = &self.header;
        let mut st = Stages::default();
        let t0 = std::time::Instant::now();
        let inflated = inflate_idat(h, self.data.as_slice(), None)?;
        let t1 = std::time::Instant::now();
        if h.interlace == 1 {
            decode_adam7(h, &inflated, buf, &mut st)?;
        } else {
            decode_scanlines(h, &inflated, buf, &mut st)?;
        }
        let t2 = std::time::Instant::now();
        eprintln!(
            "[diag] inflate={:.3}ms rows={:.3}ms total={:.3}ms inflated={}",
            t1.duration_since(t0).as_secs_f64() * 1e3,
            t2.duration_since(t1).as_secs_f64() * 1e3,
            t2.duration_since(t0).as_secs_f64() * 1e3,
            inflated.len(),
        );
        Ok(())
    }
}

/// IDAT 载荷的跨块窗口读取器（A2）：多个 IDAT 块本就是一个连续 zlib 流，但散落在输入不同偏移。
/// 以 ≤`CHUNK` 窗口顺序喂给 fdeflate，**避免多块拼接的一次整流 memcpy**——单块直接借用原始
/// 输入切片（零拷贝），多块也逐窗口借用原始切片、仅在块尾 `<8` 字节残余处做 ≤7 字节拼贴。
///
/// 喂入约束（fdeflate 语义，见 `decompress.rs` 的 8 字节粒度位缓冲）：`read` 每次只整吞 8 的
/// 倍数字节；不足 8 字节的窗口返回 (0,0) 无法推进。故本读取器保证**非流尾窗口一律为 8 的
/// 倍数**（算法：窗口取 ≤max 且对齐 8；块尾残余 <8 字节缓存到 `scratch`，与下一块头拼贴），
/// 只有全流最后一个窗口允许非对齐。
enum Window<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

struct IdatCursor<'a> {
    data: &'a [u8],
    chunks: &'a [(usize, usize)],
    /// 当前块下标。
    ci: usize,
    /// 当前块内已消费偏移。
    cpos: usize,
    /// 上一块的 <8 字节残余（跨块拼贴，最多 7 字节）。
    scratch: Vec<u8>,
}

impl<'a> IdatCursor<'a> {
    fn new(data: &'a [u8], chunks: &'a [(usize, usize)]) -> Self {
        IdatCursor {
            data,
            chunks,
            ci: 0,
            cpos: 0,
            scratch: Vec::with_capacity(8),
        }
    }

    /// 跳过已消费尽的块。
    #[inline]
    fn skip_done(&mut self) {
        while self.ci < self.chunks.len() && self.cpos >= self.chunks[self.ci].1 {
            self.ci += 1;
            self.cpos = 0;
        }
    }

    /// 当前块内剩余字节。
    #[inline]
    fn chunk_remain(&self) -> usize {
        if self.ci < self.chunks.len() {
            self.chunks[self.ci].1 - self.cpos
        } else {
            0
        }
    }

    /// 取下一窗口（≤`max`；空 = 流耗尽）。非流尾窗口是 8 的倍数。
    fn next_window(&mut self, max: usize) -> Window<'a> {
        if !self.scratch.is_empty() {
            return self.stitch(max);
        }
        self.skip_done();
        if self.ci >= self.chunks.len() {
            return Window::Borrowed(&[]);
        }
        let remain = self.chunk_remain();
        let is_last = self.ci + 1 >= self.chunks.len();
        let mut n = remain.min(max);
        if !is_last && !n.is_multiple_of(8) {
            n -= n % 8; // 对齐到 8 的倍数（非流尾时否则 fdeflate 无法吞 <8 尾）。
            if n == 0 {
                // 整块 <8：全部归入 scratch，交给拼贴窗口。
                let (off, _) = self.chunks[self.ci];
                self.scratch
                    .extend_from_slice(&self.data[off + self.cpos..off + self.cpos + remain]);
                self.cpos += remain;
                return self.next_window(max);
            }
        }
        let (off, _) = self.chunks[self.ci];
        let win = &self.data[off + self.cpos..off + self.cpos + n];
        self.cpos += n;
        Window::Borrowed(win)
    }

    /// 拼贴窗口：`scratch`(≤7) + 当前/后继块头，直到 ≥8 对齐或流尽。
    fn stitch(&mut self, max: usize) -> Window<'a> {
        let mut w = std::mem::take(&mut self.scratch);
        // 拉满到 max（跨块）。
        while w.len() < max {
            self.skip_done();
            if self.ci >= self.chunks.len() {
                break;
            }
            let remain = self.chunk_remain();
            let take = remain.min(max - w.len());
            let (off, _) = self.chunks[self.ci];
            w.extend_from_slice(&self.data[off + self.cpos..off + self.cpos + take]);
            self.cpos += take;
        }
        // 补齐到 8 的倍数（若仍有后续输入）。
        while !w.len().is_multiple_of(8) {
            self.skip_done();
            if self.ci >= self.chunks.len() {
                break;
            }
            let (off, _) = self.chunks[self.ci];
            w.push(self.data[off + self.cpos]);
            self.cpos += 1;
        }
        Window::Owned(w)
    }
}

/// 把 IDAT 解压到有界缓冲（长度 = 反滤波后线性尺寸，解析期已用 1032:1 预检防炸弹）。
///
/// 性能关键（实测 debug/release）：fdeflate 的 `read` 必须**分块（32KB，同 png crate 的
/// `CHUNK_BUFFER_SIZE`）喂入且 `end_of_input=false`**，末尾才用一次 `end_of_input=true`
/// （空输入）flush 校验 Adler-32。一次性把整段 IDAT 交给 `read`（或每调用都传 eoi=true，
/// 如 `decompress_to_vec_bounded` 内部）会触发慢路径：同样 8.3MB，1.1ms vs 10ms（~9×）。
/// 输入经 [`IdatCursor`] 逐窗口借用原始 IDAT 块（A2：多块不再拼接成连续缓冲）；输出缓冲按
/// 期望尺寸预先分配，`out_pos` 直接喂给 fdeflate 供回看引用，零重分配。后端保持 fdeflate。
fn inflate_idat(h: &FileHeader, data: &[u8], max_alloc: Option<u64>) -> ImageResult<Vec<u8>> {
    /// fdeflate 输入分块大小（同 png crate `CHUNK_BUFFER_SIZE`）。
    const CHUNK: usize = 32 * 1024;

    let expected = chunk::inflated_size(
        h.width,
        h.height,
        h.bit_depth,
        h.color_type,
        h.interlace == 1,
    );
    if let Some(max) = max_alloc
        && expected > max {
            return Err(err_limits());
        }
    let expected = usize::try_from(expected).map_err(|_| err_decode("解压尺寸超出平台表示范围"))?;

    let mut dc = fdeflate::Decompressor::new();
    // 与 png crate 解码路径一致：跳过 Adler-32 校验（其默认 ignore_adler32=true）。
    // 校验的开销在 debug 下是灾难性的大（同上 8.3MB：2.5ms vs 52ms）；zlib 流其余结构
    // 校验仍生效，目标目录与被替换的 png crate 路径行为持平。真需要校验可去掉此行。
    dc.ignore_adler32();
    let mut inflated = vec![0u8; expected];
    let mut idat = IdatCursor::new(data, h.idat_chunks.as_slice());
    let mut out_pos = 0usize;
    'main: loop {
        if dc.is_done() {
            break;
        }
        // Owned 窗口（跨块拼贴）需在本迭代保持存活以被借用喂入。
        let win = idat.next_window(CHUNK);
        let win_slice: &[u8] = match &win {
            Window::Borrowed(w) => w,
            Window::Owned(w) => w,
        };
        if win_slice.is_empty() {
            break; // 压缩输入耗尽，走末尾 flush
        }
        let mut input = win_slice;
        loop {
            if dc.is_done() {
                break 'main;
            }
            let (consumed, produced) = dc
                .read(input, &mut inflated, out_pos, false)
                .map_err(|e| err_decode(format!("IDAT 解压失败：{e:?}")))?;
            input = &input[consumed..];
            out_pos += produced;
            if input.is_empty() {
                break; // 本窗口消费完，取下一窗口
            }
            if produced == 0 {
                break 'main; // 输出已满且无进展 → 长度不符/炸弹，由末尾检查报错
            }
        }
    }
    if !dc.is_done() {
        let (_, produced) = dc
            .read(&[], &mut inflated, out_pos, true)
            .map_err(|e| err_decode(format!("IDAT 解压失败：{e:?}")))?;
        out_pos += produced;
    }
    if !dc.is_done() || out_pos != expected {
        return Err(err_decode(format!(
            "IDAT 流长度不符或未正常结束：期望 {expected} 字节，实际 {out_pos}"
        )));
    }
    Ok(inflated)
}

/// 从解压缓冲读 1 个 filter 字节（越界 = 流过短）。
#[inline]
fn next_byte(inf: &[u8], pos: &mut usize) -> ImageResult<u8> {
    if *pos >= inf.len() {
        return Err(err_decode("IDAT 流过早结束"));
    }
    let b = inf[*pos];
    *pos += 1;
    Ok(b)
}

/// 从解压缓冲取出 `len` 字节行（越界 = 流过短）。
#[inline]
fn next_row<'a>(inf: &'a [u8], pos: &mut usize, len: usize) -> ImageResult<&'a [u8]> {
    let start = *pos;
    let end = start
        .checked_add(len)
        .ok_or_else(|| err_decode("行偏移溢出"))?;
    if end > inf.len() {
        return Err(err_decode("IDAT 流过早结束"));
    }
    *pos = end;
    Ok(&inf[start..end])
}

/// EXPAND 输出色彩类型（与 png crate `output_color_type` 语义逐项对齐，含 tRNS 扩 alpha）。
pub(crate) fn output_color_type(h: &FileHeader) -> ColorType {
    use ColorType::*;
    let trns = h.has_trns;
    match (h.color_type, h.bit_depth) {
        (0, 16) => {
            if trns {
                La16
            } else {
                L16
            }
        }
        (0, _) => {
            if trns {
                La8
            } else {
                L8
            }
        }
        (2, 16) => {
            if trns {
                Rgba16
            } else {
                Rgb16
            }
        }
        (2, _) => {
            if trns {
                Rgba8
            } else {
                Rgb8
            }
        }
        (3, _) => {
            if trns {
                Rgba8
            } else {
                Rgb8
            }
        }
        (4, 16) => La16,
        (4, _) => La8,
        (6, 16) => Rgba16,
        (6, _) => Rgba8,
        _ => unreachable!("色彩类型已在前置校验"),
    }
}

// ---------------------------------------------------------------------------
// 非隔行
// ---------------------------------------------------------------------------

fn decode_scanlines(
    h: &FileHeader,
    inflated: &[u8],
    buf: &mut [u8],
    #[cfg_attr(not(feature = "stage_profile"), allow(unused_variables))] st: &mut Stages,
) -> ImageResult<()> {
    let width = h.width as usize;
    let height = h.height as usize;
    let spp = h.samples_per_pixel();
    let depth = h.bit_depth as usize;
    let fbpp = prediction_bpp(spp, depth);
    let srow = packed_row_bytes(width, spp, depth);
    let out_bpp = output_bpp(h);
    let out_row = width * out_bpp;

    let kind = expand::build_expand_kind(h);
    let table = expand::PaletteTable::from_header(h);
    let mut pos = 0usize;

    if matches!(kind, ExpandKind::Copy) {
        // 恒等路径：直接反滤波进目标缓冲（A2 src≠dst，0 额外槽）。
        // 源行活在解压缓冲 `inflated` 里，上一行取自目标缓冲（已反滤波完成），
        // 无需先拷贝一行、零中间样本分配。仅在 Copy（srow==out_row 且无展开）时成立。
        debug_assert_eq!(srow, out_row);
        let zeros = vec![0u8; srow];
        for y in 0..height {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, out_row)?;
            let (dst, above): (&mut [u8], &[u8]) = if y == 0 {
                let (_, rest) = buf.split_at_mut(0);
                let (dst, _) = rest.split_at_mut(out_row);
                (dst, &zeros)
            } else {
                let (before, rest) = buf.split_at_mut(y * out_row);
                let (dst, _) = rest.split_at_mut(out_row);
                let above = &before[(y - 1) * out_row..y * out_row];
                (dst, above)
            };
            #[cfg(feature = "stage_profile")]
            let c0 = std::time::Instant::now();
            unfilter_to(f, src, dst, above, fbpp);
            #[cfg(feature = "stage_profile")]
            let c1 = std::time::Instant::now();
            #[cfg(feature = "stage_profile")]
            {
                st.unfilter_ns += stage_probe::ns(c0, c1);
            }
        }
    } else {
        // 需要展开：行缓冲复用（A2 src≠dst 2 槽 cur/above，槽位轮流当上一行），
        // unfilter_to 把反滤波结果直接写进槽位（免拷贝），展开写目标行。
        debug_assert_eq!(buf.len(), out_row * height);
        let mut cur = vec![0u8; srow];
        let mut above = vec![0u8; srow];
        for y in 0..height {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, srow)?;
            #[cfg(feature = "stage_profile")]
            let c0 = std::time::Instant::now();
            unfilter_to(f, src, &mut cur, &above, fbpp);
            #[cfg(feature = "stage_profile")]
            let c1 = std::time::Instant::now();
            let dst = &mut buf[y * out_row..y * out_row + out_row];
            expand::expand_row(&kind, &table, &cur, dst, width);
            #[cfg(feature = "stage_profile")]
            let c2 = std::time::Instant::now();
            std::mem::swap(&mut cur, &mut above);
            #[cfg(feature = "stage_profile")]
            {
                st.unfilter_ns += stage_probe::ns(c0, c1);
                st.expand_ns += stage_probe::ns(c1, c2);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Adam7 隔行
// ---------------------------------------------------------------------------

fn decode_adam7(
    h: &FileHeader,
    inflated: &[u8],
    buf: &mut [u8],
    #[cfg_attr(not(feature = "stage_profile"), allow(unused_variables))] st: &mut Stages,
) -> ImageResult<()> {
    let width = h.width as usize;
    let height = h.height as usize;
    let spp = h.samples_per_pixel();
    let depth = h.bit_depth as usize;
    let fbpp = prediction_bpp(spp, depth);
    let out_bpp = output_bpp(h);
    let out_row = width * out_bpp;

    let kind = expand::build_expand_kind(h);
    let table = expand::PaletteTable::from_header(h);
    let max_srow = packed_row_bytes(width, spp, depth);

    let mut cur = vec![0u8; max_srow];
    let mut above = vec![0u8; max_srow];
    let mut expanded = vec![0u8; width * out_bpp];
    let mut pos = 0usize;

    // Adam7 散射快道门控（out_bpp==4 时以无边界检查 u32 直写替代逐像素拷贝），按 env 一次判定，
    // 不逐行重复读门控；仅 Adam7 路径，非隔行 decode_scanlines 不受影响。
    let fast_scatter = adam7_fast_enabled() && out_bpp == 4;

    for (x0, y0, dx, dy) in ADAM7_PASSES {
        let pw = pass_extent(width, x0, dx);
        let ph = pass_extent(height, y0, dy);
        if pw == 0 || ph == 0 {
            continue;
        }
        let srow = packed_row_bytes(pw, spp, depth);
        above[..srow].fill(0);
        for r in 0..ph {
            let f = next_byte(inflated, &mut pos)?;
            let src = next_row(inflated, &mut pos, srow)?;
            #[cfg(feature = "stage_profile")]
            let c0 = std::time::Instant::now();
            cur[..srow].copy_from_slice(src);
            #[cfg(feature = "stage_profile")]
            let c1 = std::time::Instant::now();
            let (crow, _) = cur.split_at_mut(srow);
            let (ab, _) = above.split_at(srow);
            unfilter_row(f, crow, ab, fbpp);
            #[cfg(feature = "stage_profile")]
            let c2 = std::time::Instant::now();
            let erow = &mut expanded[..pw * out_bpp];
            expand::expand_row(&kind, &table, crow, erow, pw);
            #[cfg(feature = "stage_profile")]
            let c3 = std::time::Instant::now();
            let y = y0 + r * dy;
            let dst_start = y * out_row + x0 * out_bpp;
            let erow = &expanded[..pw * out_bpp];
            if fast_scatter {
                // RGBA8（及 4B 输出）用无边界检查 u32 直写快道（scatter 阶段在落后文件占 59-70%）。
                scatter_fast(erow, &mut buf[dst_start..], pw, dx);
            } else {
                scatter(erow, &mut buf[dst_start..], pw, out_bpp, dx);
            }
            #[cfg(feature = "stage_profile")]
            let c4 = std::time::Instant::now();
            std::mem::swap(&mut cur, &mut above);
            #[cfg(feature = "stage_profile")]
            {
                st.copy_ns += stage_probe::ns(c0, c1) + stage_probe::ns(c3, c4);
                st.unfilter_ns += stage_probe::ns(c1, c2);
                st.expand_ns += stage_probe::ns(c2, c3);
            }
        }
    }

    Ok(())
}

/// 把 `count` 个（每像素 `pixel_bytes` 字节、相邻）像素写到目标行每隔 `step` 行的像素位上（Adam7 散射）。
fn scatter(src: &[u8], dst: &mut [u8], count: usize, pixel_bytes: usize, step: usize) {
    let stride = step * pixel_bytes;
    for i in 0..count {
        let s = &src[i * pixel_bytes..i * pixel_bytes + pixel_bytes];
        let d = &mut dst[i * stride..i * stride + pixel_bytes];
        d.copy_from_slice(s);
    }
}

/// Adam7 散射快道：`pixel_bytes==4`（RGBA8 等 4B 输出）时用 native 序 u32 无边界检查直写。
/// 语义与慢道逐字节拷贝恒等——u32 读/写不重排四字节，双端字节序等价（测试 `scatter_fast_parity` 实锤）。
/// 安全性：`src` 长度 = `count*4`；pass 几何保证末像素写入止于 `(count-1)*stride+4`，其
/// `<= dst.len()`（末列 `x0+(count-1)*dx < width` 恒成立，见 `decode_adam7` 的 `pass_extent`）。
/// 注意：本快道**非 SIMD**（raw u32 直写，纯内存拷贝优化）；`KIMG_PNG_NO_SIMD=1` 测量旋钮语义
/// 只关 SIMD 路径（反滤波/展开），故不经其门控——字节恒等，无歧义风险。
#[inline]
fn scatter_fast(src: &[u8], dst: &mut [u8], count: usize, step: usize) {
    let stride = step * 4;
    let sp = src.as_ptr();
    let dp = dst.as_mut_ptr();
    debug_assert!(src.len() >= count * 4);
    debug_assert!(count == 0 || (count - 1) * stride + 4 <= dst.len());
    for i in 0..count {
        unsafe {
            let v = std::ptr::read_unaligned(sp.add(i * 4) as *const u32);
            std::ptr::write_unaligned(dp.add(i * stride) as *mut u32, v);
        }
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;

    /// 构建一个最小合法 PNG（ct/depth 自定，1 行数据）。
    fn make_mini_png(ct: u8, depth: u8, row_data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let crc = |bytes: &[u8]| {
            let mut h = crc32fast::Hasher::new();
            h.update(bytes);
            h.finalize()
        };
        let chunk = |name: &[u8; 4], payload: &[u8]| -> Vec<u8> {
            let mut c = Vec::new();
            c.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            c.extend_from_slice(name);
            c.extend_from_slice(payload);
            let mut cov = name.to_vec();
            cov.extend_from_slice(payload);
            c.extend_from_slice(&crc(&cov).to_be_bytes());
            c
        };
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[depth, ct, 0, 0, 0]);
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(row_data).unwrap();
        let z = enc.finish().unwrap();
        out.extend_from_slice(&chunk(b"IDAT", &z));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    /// A1：零拷贝路径的 `Input` 变体断言 + 三路（借用/自有解析/自有 vec）输出逐字节一致。
    #[test]
    fn input_borrow_vs_owned_paths_identical() {
        // 1x1 RGBA：filter 0 + 4 样本。
        let png = make_mini_png(6, 8, &[0, 10, 20, 30, 255]);

        let borrowed = PngEngine::from_slice(&png).unwrap();
        assert!(
            matches!(borrowed.data, Input::Borrowed(_)),
            "from_slice 必须走借用路径"
        );
        assert_eq!(borrowed.data.as_slice(), png.as_slice());

        let owned_slice = PngEngine::from_vec(png.clone()).unwrap();
        assert!(
            matches!(owned_slice.data, Input::Owned(_)),
            "from_vec 必须走自有路径"
        );

        let mut parsed = std::io::Cursor::new(&png[..]);
        let owned_read = PngEngine::parse_with_capacity(&mut parsed, png.len()).unwrap();
        assert!(
            matches!(owned_read.data, Input::Owned(_)),
            "读取器路径必须走自有路径"
        );

        // 输出逐字节一致。
        let mut b1 = vec![0u8; borrowed.total_bytes() as usize];
        let mut b2 = vec![0u8; owned_slice.total_bytes() as usize];
        let mut b3 = vec![0u8; owned_read.total_bytes() as usize];
        borrowed.decode(&mut b1, None).unwrap();
        owned_slice.decode(&mut b2, None).unwrap();
        owned_read.decode(&mut b3, None).unwrap();
        assert_eq!(b1, b2, "from_slice 与 from_vec 输出不一致");
        assert_eq!(b1, b3, "from_slice 与读取器路径输出不一致");
        assert_eq!(b1, [10, 20, 30, 255]);
    }
}

#[cfg(test)]
mod diag_tests {
    use super::*;

    /// 分阶段计时诊断：RGBA8 大图（跑 `cargo test -p kimg --lib pngsimd::diag -- --ignored --nocapture`）。`#[ignore]` 手动。
    #[test]
    #[ignore = "手动运行"]
    fn decode_phase_timing_rgba8() {
        let w = 1920u32;
        let h = 1080u32;
        let row = w as usize * 4;
        let mut img = Vec::with_capacity(row * h as usize);
        let mut lcg = 0x9E3779B97F4A7C15u64;
        for _ in 0..(row * h as usize) {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            img.push((lcg >> 40) as u8);
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&img).unwrap();
        }
        // 抽出 IDAT（首个）载荷 = 真正的 zlib 流。
        let idat = {
            let mut pos = 8usize;
            loop {
                let len = u32::from_be_bytes([
                    bytes[pos],
                    bytes[pos + 1],
                    bytes[pos + 2],
                    bytes[pos + 3],
                ]) as usize;
                if &bytes[pos + 4..pos + 8] == b"IDAT" {
                    break bytes[pos + 8..pos + 8 + len].to_vec();
                }
                pos += 12 + len;
            }
        };

        // 1) decompress_to_vec_bounded
        let expected = 8_295_480usize;
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = fdeflate::decompress_to_vec_bounded(&idat, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] bounded helper = {durations:?}ms");

        // 2) 手动 Decompressor 循环（固定 64KB 输出缓冲）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut input: &[u8] = &idat;
            let mut output = vec![0u8; 64 * 1024];
            let mut total_out = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = match dc.read(&input, &mut output, 0, true) {
                    Ok(v) => v,
                    Err(e) => {
                        panic!("diag inflate err {e:?}");
                    }
                };
                input = &input[consumed..];
                total_out += produced;
            }
            durations.push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(total_out, expected);
        }
        eprintln!("[diag-inflate] manual loop 64KB = {durations:?}ms");

        // 2b) 两阶段（eoi=false 流式 + 末尾 eoi=true flush）+ ignore_adler32
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            dc.ignore_adler32();
            let mut inflated = vec![0u8; expected];
            let mut input: &[u8] = &idat;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = dc.read(&input, &mut inflated, out_pos, false).unwrap();
                input = &input[consumed..];
                out_pos += produced;
                if consumed == 0 && produced == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase + ignore_adler32 = {durations:?}ms");

        // 2c) 两阶段不 ignore_adler32（与 2b 仅差 adler 校验）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut inflated = vec![0u8; expected];
            let mut input: &[u8] = &idat;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let (consumed, produced) = dc.read(&input, &mut inflated, out_pos, false).unwrap();
                input = &input[consumed..];
                out_pos += produced;
                if consumed == 0 && produced == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase + adler = {durations:?}ms");

        // 2d) 32KB 分块喂入（png 同款节奏）+ ignore_adler32
        let mut durations = Vec::new();
        const CHUNK: usize = 32 * 1024;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            dc.ignore_adler32();
            let mut inflated = vec![0u8; expected];
            let mut in_off = 0usize;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let end = (in_off + CHUNK).min(idat.len());
                let (consumed, produced) = dc
                    .read(&idat[in_off..end], &mut inflated, out_pos, false)
                    .unwrap();
                in_off += consumed;
                out_pos += produced;
                if end == idat.len() && consumed == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase chunked32k + ignore_adler32 = {durations:?}ms");

        // 2e) 32KB 分块 + 保留 adler 校验（确认 adler 是否是慢源）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let mut dc = fdeflate::Decompressor::new();
            let mut inflated = vec![0u8; expected];
            let mut in_off = 0usize;
            let mut out_pos = 0usize;
            loop {
                if dc.is_done() {
                    break;
                }
                let end = (in_off + CHUNK).min(idat.len());
                let (consumed, produced) = dc
                    .read(&idat[in_off..end], &mut inflated, out_pos, false)
                    .unwrap();
                in_off += consumed;
                out_pos += produced;
                if end == idat.len() && consumed == 0 {
                    break;
                }
            }
            if !dc.is_done() {
                let (_, produced) = dc.read(&[], &mut inflated, out_pos, true).unwrap();
                out_pos += produced;
            }
            assert_eq!(out_pos, expected);
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] two-phase chunked32k + adler = {durations:?}ms");
        // 3) png crate 全解码（参照）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let dec = png::Decoder::new(&bytes[..]);
            let mut r = dec.read_info().unwrap();
            let mut out = vec![0u8; r.output_buffer_size()];
            r.next_frame(&mut out).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-inflate] png crate full decode = {durations:?}ms");

        // 引擎实测
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            PngEngine::parse(&mut &bytes[..]).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!(
            "[diag-parse] parse (whole file) = {durations:?}ms  input={}B",
            bytes.len()
        );

        // parse 归因：fill_buf 单次 memcpy vs parse_png 扫描。
        {
            use std::io::BufRead as _;
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let mut data = Vec::new();
                data.reserve(bytes.len());
                let mut r: &[u8] = &bytes;
                loop {
                    let buf = r.fill_buf().unwrap();
                    if buf.is_empty() {
                        break;
                    }
                    let n = buf.len();
                    data.extend_from_slice(buf);
                    r.consume(n);
                }
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag-parse] fill_buf 单次 memcpy only = {durations:?}ms");
            // 对照：to_owned（整块 memcpy）与 copy_nonoverlapping。
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let _data = bytes.to_owned();
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag-parse] to_owned（整块） = {durations:?}ms");
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let mut data = Vec::with_capacity(bytes.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.as_mut_ptr(), bytes.len());
                    data.set_len(bytes.len());
                }
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag-parse] copy_nonoverlapping = {durations:?}ms");
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let _ = chunk::parse_png(&bytes).unwrap();
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag-parse] parse_png only = {durations:?}ms");
        }

        // 输出分配（vec![0; N] debug 零化）
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let _ = vec![0u8; 8_295_480];
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-parse] alloc+zero 8.3MB = {durations:?}ms");

        // 引擎实测
        let engine = PngEngine::parse(&mut &bytes[..]).unwrap();
        let mut buf = vec![0u8; engine.total_bytes() as usize];
        for _ in 0..3 {
            engine.decode_timed(&mut buf).unwrap();
        }

        // 生产路径实测（PngDecoder::new 走 with_limits 的预留容量解析 + read_image 全解码）。
        {
            use crate::ImageDecoder as _;
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let dec =
                    crate::codecs::png::PngDecoder::new(std::io::Cursor::new(&bytes[..])).unwrap();
                let mut out = vec![0u8; dec.total_bytes() as usize];
                dec.read_image(&mut out).unwrap();
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag] PngDecoder::new + read_image（生产路径） = {durations:?}ms");
            // 只测构造（解析 + 预留容量 read_to_end），不含解码。
            let mut durations = Vec::new();
            for _ in 0..3 {
                let t = std::time::Instant::now();
                let _ =
                    crate::codecs::png::PngDecoder::new(std::io::Cursor::new(&bytes[..])).unwrap();
                durations.push(t.elapsed().as_secs_f64() * 1e3);
            }
            eprintln!("[diag] PngDecoder::new 仅构造（解析+预留 read_to_end） = {durations:?}ms");
        }
    }

    /// 分阶段计时诊断：gray8 1920x1080（bench 同款 `photo_bytes`）inflate vs rows 拆分 + filter 分布。
    /// `#[ignore]` 手动：`cargo test -p kimg --release --lib pngsimd::diag -- --ignored --nocapture`。
    #[test]
    #[ignore = "手动运行"]
    fn decode_phase_timing_gray8() {
        let (w, h) = (1920u32, 1080u32);
        let mut img = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let base = ((x as f64 * 1.3 + y as f64 * 0.7) as usize & 0x3F) as u8;
                let s = (x as f64 / 37.0 * std::f64::consts::PI * 2.0).sin();
                let s2 = (y as f64 / 41.0 + x as f64 / 97.0) * std::f64::consts::PI * 2.0;
                let v = (base as f64 + 96.0 * s + 64.0 * s2.sin() + 48.0).clamp(0.0, 255.0) as u8;
                img.push(v);
            }
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&img).unwrap();
        }
        // filter 分布：解压后逐行取 filter 字节。
        let engine = PngEngine::parse(&mut &bytes[..]).unwrap();
        let inflated = inflate_idat(&engine.header, engine.data.as_slice(), None).unwrap();
        let srow = packed_row_bytes(engine.header.width as usize, 1, 8);
        let mut counts = [0u32; 5];
        let mut pos = 0usize;
        for _y in 0..engine.header.height as usize {
            let f = inflated[pos];
            if f < 5 {
                counts[f as usize] += 1;
            }
            pos += 1 + srow;
        }
        eprintln!("[diag-gray8] filter 分布(逐行): None/Sub/Up/Avg/Paeth = {counts:?}");

        let mut buf = vec![0u8; engine.total_bytes() as usize];
        for _ in 0..3 {
            engine.decode_timed(&mut buf).unwrap();
        }
        // png crate 参照全解码。
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let dec = png::Decoder::new(&bytes[..]);
            let mut r = dec.read_info().unwrap();
            let mut out = vec![0u8; r.output_buffer_size()];
            r.next_frame(&mut out).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-gray8] png crate 全解码 = {durations:?}ms");
    }

    /// 4K（3840×2160 rgba8）分阶段计时：inflate vs rows 的占比——决定行级并行（rayon）是否有实益。
    /// `#[ignore]` 手动。若 inflate（单线程）占大头，行级并行的最大收益被卡死。
    #[test]
    #[ignore = "手动运行"]
    fn decode_phase_timing_4k() {
        let (w, h) = (3840u32, 2160u32);
        let row = w as usize * 4;
        let mut img = Vec::with_capacity(row * h as usize);
        let mut lcg = 0x9E3779B97F4A7C15u64;
        for _ in 0..(row * h as usize) {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            img.push((lcg >> 40) as u8);
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&img).unwrap();
        }
        let engine = PngEngine::parse(&mut &bytes[..]).unwrap();
        let mut buf = vec![0u8; engine.total_bytes() as usize];
        for _ in 0..3 {
            engine.decode_timed(&mut buf).unwrap();
        }
    }

    /// RGB8（bpp3）1920x1080 分阶段计时：inflate vs rows——定位 rgb8「略慢」的gap在解压还是行处理。
    /// `#[ignore]` 手动。
    #[test]
    #[ignore = "手动运行"]
    fn decode_phase_timing_rgb8() {
        let (w, h) = (1920u32, 1080u32);
        let row = w as usize * 3;
        let mut img = Vec::with_capacity(row * h as usize);
        let mut lcg = 0x9E3779B97F4A7C15u64;
        for _ in 0..(row * h as usize) {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            img.push((lcg >> 40) as u8);
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&img).unwrap();
        }
        let engine = PngEngine::parse(&mut &bytes[..]).unwrap();
        // filter 分布（解压后逐行取 filter 字节）。
        {
            let inflated = inflate_idat(&engine.header, engine.data.as_slice(), None).unwrap();
            let srow = packed_row_bytes(engine.header.width as usize, 3, 8);
            let mut counts = [0u32; 5];
            let mut pos = 0usize;
            for _y in 0..engine.header.height as usize {
                let f = inflated[pos];
                if f < 5 {
                    counts[f as usize] += 1;
                }
                pos += 1 + srow;
            }
            eprintln!("[diag-rgb8] filter 分布(逐行): None/Sub/Up/Avg/Paeth = {counts:?}");
        }
        let mut buf = vec![0u8; engine.total_bytes() as usize];
        for _ in 0..3 {
            engine.decode_timed(&mut buf).unwrap();
        }
        let mut durations = Vec::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let dec = png::Decoder::new(&bytes[..]);
            let mut r = dec.read_info().unwrap();
            let mut out = vec![0u8; r.output_buffer_size()];
            r.next_frame(&mut out).unwrap();
            durations.push(t.elapsed().as_secs_f64() * 1e3);
        }
        eprintln!("[diag-rgb8] png crate 全解码 = {durations:?}ms");
    }

    /// 分阶段计时诊断：纯标量、纯 SIMD 反滤波对照。`#[ignore]` 手动。
    #[test]
    #[ignore = "手动运行"]
    fn unfilter_rate_rgba8() {
        // 直接生成一个模拟行缓冲（随机 + 固定 filter）测速率。
        let n = 1920 * 4usize;
        let rows = 1080usize;
        // 总工作量 ~13MB。
        let mut t_simd = std::time::Duration::ZERO;
        let mut t_scal = std::time::Duration::ZERO;
        for filter in 1..=4u8 {
            let mut row = vec![0u8; n];
            let above = vec![0u8; n];
            let mut lcg = 0xABCDEF12345678u64;
            for b in row.iter_mut() {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (lcg >> 40) as u8;
            }

            let t0 = std::time::Instant::now();
            for _ in 0..rows {
                unfilter_row(filter, &mut row, &above, 4);
            }
            t_simd += t0.elapsed();
            let t1 = std::time::Instant::now();
            for _ in 0..rows {
                scal_avg_etc(filter, &mut row, &above, 4);
            }
            t_scal += t1.elapsed();
            eprintln!(
                "[diag-unfilter] filter={filter} simd={:?} scalar={:?}",
                t_simd, t_scal
            );
        }
        eprintln!("[diag-unfilter] simd={:?} scalar={:?}", t_simd, t_scal);
    }

    fn scal_avg_etc(filter: u8, row: &mut [u8], above: &[u8], bpp: usize) {
        // 纯标量反滤波（与 unfilter_row 对照），慢速参考。
        for i in 0..row.len() {
            match filter {
                1 => row[i] = row[i].wrapping_add(row[i.wrapping_sub(bpp.max(1)).min(i)]),
                2 => row[i] = row[i].wrapping_add(above[i]),
                3 => {
                    let l = if i >= bpp { row[i - bpp] } else { 0 };
                    row[i] = row[i].wrapping_add(((u16::from(l) + u16::from(above[i])) >> 1) as u8)
                }
                _ => {
                    let l = if i >= bpp { i32::from(row[i - bpp]) } else { 0 };
                    let a = i32::from(above[i]);
                    let c = if i >= bpp {
                        i32::from(above[i - bpp])
                    } else {
                        0
                    };
                    let p = l + a - c;
                    let pa = (p - l).abs();
                    let pb = (p - a).abs();
                    let pc = (p - c).abs();
                    let pred = if pa <= pb && pa <= pc {
                        l
                    } else if pb <= pc {
                        a
                    } else {
                        c
                    };
                    row[i] = row[i].wrapping_add(pred as u8);
                }
            }
        }
    }
}

#[cfg(test)]
mod adam7_fast_tests {
    use super::*;
    use std::path::Path;

    /// 语料目录：默认 `<workspace>/Benchmarks/corpus/PNG`；env `KIMG_PNG_CORPUS` 优先覆盖
    /// （装配统一走 `kimg_core::testutil::corpus_dir`，键名/默认路径/缺失语义不变）。

    /// 3 个 adam7 落后文件 + 25__adam7（proto 分账证明 scatter 为其临界项）。
    const ADAM7_FILES: [&str; 4] = [
        "sdcb_screen_p8a_adam7.png",
        "sdcb_screen_rgba8_adam7.png",
        "sdcb_photo_p8a_adam7.png",
        "25__adam7.png",
    ];

    /// 确定性字节模式（防跨进程校验依赖污染）。
    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        if seed == 0 {
            let mut v = Vec::with_capacity(len);
            for i in 0..len {
                v.push((i % 251) as u8);
            }
            return v;
        }
        let mut lcg = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
        (0..len)
            .map(|_| {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (lcg >> 41) as u8
            })
            .collect()
    }

    /// 对 (count,step) 组合跑「慢道 scatter ≡ 快道 scatter_fast」逐字节断言。
    fn assert_parity_pair(count: usize, step: usize, seed: u64) {
        // 目标缓冲留 slack：证明快道不越界多写（slack 两侧均保持零）。
        let dst_len = count * step * 4 + 16;
        let src = pattern(count * 4, seed);
        let mut slow = vec![0u8; dst_len];
        let mut fast = vec![0u8; dst_len];
        // slow 侧固定 4B/px（scatter 签名带 pixel_bytes 参数，fast 为 4B 专用签名）；
        // 两侧语义一致：每像素 4 字节、以 `step*4` 步进散射。
        scatter(&src, &mut slow, count, 4, step);
        scatter_fast(&src, &mut fast, count, step);
        assert_eq!(
            slow, fast,
            "scatter(慢) ≡ scatter_fast(快) 失配 count={count} step={step} seed={seed:#x}"
        );
    }

    /// 直拍 1：自构造边角——Adam7 全 7 pass 几何 × 各种宽（1、7、非 8 倍数……）+ 混合字节模式，
    /// 覆盖 pass 步幅 {8,4,2,1}、count 边角（1、7、8、9、非 8 倍数、大数），逐字节 0 失配。
    #[test]
    fn scatter_fast_parity() {
        let widths = [1usize, 2, 7, 8, 9, 13, 16, 17, 167, 1920];
        // Adam7 七遍 (x0, step, dx 列步幅)：(x0=0,8)、(4,8)、(0,4)、(2,4)、(0,2)、(1,2)、(0,1)。
        let passes = [
            (0usize, 8usize),
            (4, 8),
            (0, 4),
            (2, 4),
            (0, 2),
            (1, 2),
            (0, 1),
        ];
        let mut combos: Vec<(usize, usize)> = Vec::new();
        for &w in &widths {
            for &(x0, step) in &passes {
                let count = pass_extent(w, x0, step);
                if count > 0 {
                    combos.push((count, step));
                }
            }
        }
        // 补充稀疏大 count（覆盖较长重复步进写）。
        combos.push((1344, 1));
        combos.push((168, 8));
        combos.push((169, 4));
        combos.push((673, 2));
        combos.push((1, 1));
        combos.push((7, 2));
        combos.push((0, 1));

        let seeds = [
            0u64, // 循环 ramp（含重复字节区间，抓字节重排）
            1,    // LCG 伪随机
            0xDEAD_BEEF, 0xFFFFFFFF_FFFFFFFF, 0x0102_0304, 0x7F7F_7F7F,
        ];
        let mut checked = 0usize;
        for &(count, step) in &combos {
            for &seed in &seeds {
                assert_parity_pair(count, step, seed);
                checked += 1;
            }
        }
        eprintln!("[adam7-fast] spread scatter_fast parity: {checked} 组 (count,step,seed) 全绿");
    }

    /// 对单个文件解码并返回 FNV-1a 64 摘要（无依赖、跨进程确定）。
    fn decode_hash(path: &Path) -> Option<(usize, u64)> {
        let bytes = std::fs::read(path).ok()?;
        let eng = PngEngine::from_vec(bytes).ok()?;
        let n = eng.total_bytes() as usize;
        let mut buf = vec![0u8; n];
        eng.decode(&mut buf, None).ok()?;
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in &buf {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        Some((n, h))
    }

    /// 子进程慢道 dump 助手：父测试以 `KIMG_PNG_ADAM7_FAST=0` 派生本测试，打印每文件慢道摘要。
    /// 正常随套件跑（fast 态）时仅解码打印，无副作用。
    #[test]
    fn scatter_fast_slow_dump_helper() {
        let Some(dir) = kimg_core::testutil::corpus_dir("KIMG_PNG_CORPUS", "PNG") else {
            eprintln!("SKIP: missing corpus/PNG");
            return;
        };
        for name in ADAM7_FILES {
            let p = dir.join(name);
            match decode_hash(&p) {
                Some((n, h)) => println!("ADAM7_SLOW_HASH {name} {n} {h:016x}"),
                None => eprintln!("SKIP: missing {}", p.display()),
            }
        }
    }

    /// 直拍 2：真·快道≡慢道对 3 个落后 adam7 文件 + 25__adam7 逐字节 0 失配。
    /// 本进程按当前 env 解码（默认快道）；子进程强制 `KIMG_PNG_ADAM7_FAST=0` 走慢道，
    /// 两侧摘要必须一致（env 门控进程级缓存，故跨进程对拍）。
    /// 注意：若整套餐件本身以 `KIMG_PNG_ADAM7_FAST=0` 运行，本进程与子进程同走慢道，
    /// 断言退化为 slow-vs-slow 恒等（防误读：真对拍只在默认快态下成立）。
    #[test]
    fn scatter_fast_real_files_matches_slow() {
        let Some(dir) = kimg_core::testutil::corpus_dir("KIMG_PNG_CORPUS", "PNG") else {
            eprintln!("SKIP: missing corpus/PNG");
            return;
        };
        // 本进程快道摘要（若整套餐件在 env=0 下运行则为慢道，跨进程对拍仍成立）。
        let mut fast_hashes: Vec<(String, usize, u64)> = Vec::new();
        for name in ADAM7_FILES {
            let p = dir.join(name);
            match decode_hash(&p) {
                Some((n, h)) => fast_hashes.push((name.to_string(), n, h)),
                None => {
                    eprintln!("SKIP: 解码失败 {}", p.display());
                    return;
                }
            }
        }

        let exe = std::env::current_exe().expect("当前测试可执行路径");
        // module_path!() 带 crate 名前缀（`.e.g.kimg_png::...`），libtest 全名不带——剥前缀对齐。
        let crate_pref = format!("{}::", env!("CARGO_PKG_NAME"));
        let helper = format!(
            "{}::scatter_fast_slow_dump_helper",
            module_path!()
        );
        let helper = helper.strip_prefix(&crate_pref).unwrap_or(&helper);
        let out = std::process::Command::new(&exe)
            .args(["--exact", &helper, "--nocapture", "--test-threads=1"])
            .env("KIMG_PNG_ADAM7_FAST", "0")
            .env("KIMG_PNG_CORPUS", &dir)
            .output()
            .expect("子进程（慢道）运行失败");
        assert!(
            out.status.success(),
            "子进程（慢道）失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut slow: std::collections::HashMap<String, (usize, u64)> =
            std::collections::HashMap::new();
        for line in stdout.lines() {
            // 首行会被 libtest「test <名> ...」前缀占用——按标记 token 出现位置解析，勿断言行首。
            let toks: Vec<&str> = line.split_whitespace().collect();
            let i = match toks.iter().position(|&t| t == "ADAM7_SLOW_HASH") {
                Some(i) => i,
                None => continue,
            };
            if toks.len() < i + 4 {
                continue;
            }
            let name = toks[i + 1].to_string();
            let n: usize = toks[i + 2].parse().unwrap_or(0);
            let h = u64::from_str_radix(toks[i + 3], 16).unwrap_or(0);
            slow.insert(name, (n, h));
        }
        assert_eq!(
            slow.len(),
            ADAM7_FILES.len(),
            "子进程未产出全部慢道摘要：\n{}",
            stdout
        );
        for (name, n, h) in &fast_hashes {
            let (sn, sh) = slow.get(name).unwrap_or_else(|| panic!("缺慢道摘要 {name}"));
            assert_eq!(
                (n, h),
                (&(*sn), sh),
                "快道≠慢道：{name} 字节不一致（{n} bytes vs {sn}）"
            );
        }
        eprintln!(
            "[adam7-fast] 真机语料快道≡慢道：{} 文件全绿",
            fast_hashes.len()
        );
    }
}
