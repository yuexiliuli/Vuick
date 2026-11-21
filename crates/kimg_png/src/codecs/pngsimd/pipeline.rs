//! 双核管道解码（P-B）：生产者线程跑 fdeflate 解压为**单调整帧** `inflated`，消费者（decode
//! 主线程）顺序逐行在上反滤波+展开，两核并行、垂直行依赖由顺序消费满足。
//!
//! 正确性硬约束（对抗评审 R4-4 与 R4-1）：
//! - fdeflate 0.3.7 `Decompressor` **无内部窗口**，backref 直接读调用方输出切片
//!   （`decompress.rs:799 dist > output_index → DistanceTooFarBack`）——**禁止**复用小缓冲 +
//!   `output_pos=0`；本实现保持「单调整帧 `inflated: Vec<u8>`、`out_pos` 单调递增」，管道只是
//!   在其上把解压与行处理拆到两线程，不省整帧内存（B 只赚墙钟）。
//! - 跨线程共享同一帧：`InflateShared.data` 用 `UnsafeCell`；无数据竞争靠「生产者只写
//!   `[ready, ·)`、消费者只读 `[0, ready)`」+ `AtomicUsize` Release/Acquire 成立（互斥区间不相交）。
//! - 任意一侧 Err → 双端收敛：消费者错误后仍 join 生产者（scope 收尾到 drop sender、不背压
//!   死锁）；生产者错误存 `InflateShared.err` 后退出；错误合并用 `Result::or`。禁止 panic 路径。
//! - 末尾长度校验保留（`out_pos == expected`）。
//!
//! 开关（每次解码读一次 env，测试可翻转）：`KIMG_PNG_LEVEL`=A/B/auto（强制车道）、
//! `KIMG_PNG_PIPELINE`=0/1/auto（显式开关；auto=阈值）。仅非隔行走管道（Adam7 保持顺序）。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, MutexGuard};

/// 取 `InflateShared::err` 槽（Mutex 中毒时吞掉 poison 取回值）。panic=abort 下本不应有
/// 中毒路径，此举使「全程无 panic 路径」承诺成立（不再有任何 `unwrap` 派生 panic 入口）。
fn err_slot(shared: &InflateShared) -> MutexGuard<'_, Option<String>> {
    shared
        .err
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

use crate::codecs::pngsimd::chunk::FileHeader;
use crate::codecs::pngsimd::expand::{ExpandKind, PaletteTable, build_expand_kind, output_bpp};
use crate::codecs::pngsimd::predict::{packed_row_bytes, prediction_bpp, unfilter_to};
use kimg_core::error::ImageResult;

use super::{IdatCursor, Stages, Window, err_decode};

/// 管道输入阈值：压缩字节 ≥512KB 或 输出字节 ≥4MB 才开管道（小图线程开销 > 收益）。
pub(crate) const PIPELINE_MIN_COMPRESSED: usize = 512 * 1024;
pub(crate) const PIPELINE_MIN_INFLATED: usize = 4 * 1024 * 1024;

/// fdeflate 输入分块大小（同 png crate `CHUNK_BUFFER_SIZE`）。
const CHUNK: usize = 32 * 1024;

/// 车道模式（`KIMG_PNG_LEVEL`）：A=顺序单线程；B=双核管道；auto=按阈值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LevelMode {
    A,
    B,
    Auto,
}

/// 管道开关（`KIMG_PNG_PIPELINE`）：0=关；1=开；auto=按阈值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PipeMode {
    Off,
    On,
    Auto,
}

/// 车道模式（每次解码读取一次；测试可经 env 翻转）。
pub(crate) fn level_mode() -> LevelMode {
    match std::env::var("KIMG_PNG_LEVEL").ok().as_deref() {
        Some(v) if v.eq_ignore_ascii_case("a") => LevelMode::A,
        Some(v) if v.eq_ignore_ascii_case("b") => LevelMode::B,
        _ => LevelMode::Auto,
    }
}

/// 管道开关（每次解码读取一次）。
pub(crate) fn pipe_mode() -> PipeMode {
    match std::env::var("KIMG_PNG_PIPELINE").ok().as_deref() {
        Some(v) if v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("off") => PipeMode::Off,
        Some(v) if v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("on") => PipeMode::On,
        _ => PipeMode::Auto,
    }
}

/// 决定是否走双核管道：仅非隔行，且车道/开关允许。
pub(crate) fn pipeline_enabled(h: &FileHeader, compressed: usize, inflated: usize) -> bool {
    if h.interlace == 1 {
        return false; // 仅非隔行（Adam7 保持顺序）
    }
    match level_mode() {
        LevelMode::B => return pipe_mode() != PipeMode::Off,
        LevelMode::A => return false,
        LevelMode::Auto => {}
    }
    match pipe_mode() {
        PipeMode::On => true,
        PipeMode::Off => false,
        PipeMode::Auto => {
            compressed >= PIPELINE_MIN_COMPRESSED || inflated >= PIPELINE_MIN_INFLATED
        }
    }
}

/// 双核共享帧缓冲（生产者写、消费者读）。
pub(crate) struct InflateShared {
    /// 单调整帧解压缓冲（长度 = `expected`；生产者 `&mut`、消费者 `&` 借用）。
    data: UnsafeCell<Vec<u8>>,
    /// 已提交可读字节数（生产者 `Release` 发布 / 消费者 `Acquire` 读取）。
    ready: AtomicUsize,
    /// 生产者错误（String 便于跨线程；None = 未错）。
    err: Mutex<Option<String>>,
}

impl InflateShared {
    pub(crate) fn new(expected: usize) -> Self {
        InflateShared {
            data: UnsafeCell::new(vec![0u8; expected]),
            ready: AtomicUsize::new(0),
            err: Mutex::new(None),
        }
    }

    /// 消费者的只读视图（仅读 `[0, ready)` 前缀；经 `ready` Acquire 同步）。
    #[inline]
    pub(crate) unsafe fn as_slice(&self) -> &[u8] {
        unsafe { &*self.data.get() }
    }

    /// 读取已提交字节数（Acquire）。
    #[inline]
    fn ready_load(&self) -> usize {
        self.ready.load(Ordering::Acquire)
    }
}

// SAFETY 契约（模块头已述）：生产者只写 `[ready, ·)`、消费者只读 `[0, ready)`，二者区间不相交，
// 交替经 `ready` Release/Acquire 同步（无 C11 数据竞争）；`err` 字段由 `Mutex` 保护。
// `UnsafeCell` 本身非 Sync，故按此契约手动声明。
unsafe impl Sync for InflateShared {}

/// 生产者：fdeflate 顺序膨胀到单调整帧 `inflated`，每次 `read` 后 `Release` 发布 `ready` 并经
/// 有界通道唤醒消费者（通道同时充当背压与错误收敛信号）。任何错误改存 `err` 后退出。
fn inflate_producer(
    h: &FileHeader,
    data: &[u8],
    max_alloc: Option<u64>,
    shared: &InflateShared,
    tx: SyncSender<usize>,
) {
    let expected = {
        let e = crate::codecs::pngsimd::chunk::inflated_size(
            h.width,
            h.height,
            h.bit_depth,
            h.color_type,
            h.interlace == 1,
        );
        if let Some(max) = max_alloc
            && e > max {
                *err_slot(shared) = Some("解压尺寸超出限制".to_string());
                return;
            }
        match usize::try_from(e) {
            Ok(v) => v,
            Err(_) => {
                *err_slot(shared) = Some("解压尺寸超出平台表示范围".to_string());
                return;
            }
        }
    };

    let mut dc = fdeflate::Decompressor::new();
    dc.ignore_adler32();
    let out: &mut [u8] = unsafe { &mut *shared.data.get() }; // 单调整帧（生产者独占 &mut）
    let mut idat = IdatCursor::new(data, h.idat_chunks.as_slice());
    let mut out_pos = 0usize;
    let publish = |pos: usize| {
        shared.ready.store(pos, Ordering::Release);
        let _ = tx.send(pos);
    };

    'main: loop {
        if dc.is_done() {
            break;
        }
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
            let (consumed, produced) = match dc.read(input, out, out_pos, false) {
                Ok(v) => v,
                Err(e) => {
                    *err_slot(shared) = Some(format!("IDAT 解压失败：{e:?}"));
                    return;
                }
            };
            input = &input[consumed..];
            out_pos += produced;
            // 不变式（C11 无数据竞争的区间依据）：生产者只写 [ready, ·)，消费者只读 [0, ready)。
            // fdeflate 保证 produced ≤ expected - out_pos 前值，out_pos 永不超过期望长度；
            // debug 下把该前提钉死，防任一读偏越界发布造成消费者越界索引。
            debug_assert!(out_pos <= expected, "inflate 产出越过单调整帧");
            // 仅「有进展」才发布 + 唤醒（out_pos 不变的重发会填满消费者已停读的有界通道 →
            // 生产者 join 死锁）。
            if produced > 0 {
                publish(out_pos);
            }
            if input.is_empty() {
                break; // 本窗口消费完，取下一窗口
            }
            if produced == 0 {
                break 'main; // 输出已满且无进展 → 长度不符/炸弹，由末尾检查报错
            }
        }
    }
    if !dc.is_done() {
        match dc.read(&[], out, out_pos, true) {
            Ok((_, produced)) => {
                out_pos += produced;
                if produced > 0 {
                    publish(out_pos);
                }
            }
            Err(e) => {
                *err_slot(shared) = Some(format!("IDAT 解压失败：{e:?}"));
                return;
            }
        }
    }
    if !dc.is_done() || out_pos != expected {
        *err_slot(shared) = Some(format!(
            "IDAT 流长度不符或未正常结束：期望 {expected} 字节，实际 {out_pos}"
        ));
    }
    // 返回即 drop `tx` → 消费者 `recv()` 返回 `Err(RecvError)` 收尾。
}

/// 管道消费者（decode 主线程）：顺序逐行在共享帧上 unfilter+expand。
/// 行 `y` 的字节（含 filter 字节）就绪（`ready` 覆盖行尾）前阻塞等通道唤醒。
/// 任一行出错 → 返回错误；随后 `thread::scope` 仍 join 生产者（错误收敛、防背压死锁）。
fn consume_rows(
    h: &FileHeader,
    shared: &InflateShared,
    rx: &Receiver<usize>,
    buf: &mut [u8],
    kind: &ExpandKind,
    table: &PaletteTable,
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
    let stride = srow + 1; // 每行 = 1 filter 字节 + srow 数据字节

    let mut ready = 0usize;
    let mut finalized = false;
    let mut y = 0usize;

    if matches!(kind, ExpandKind::Copy) {
        // 恒等路径：直接反滤波到目标行（0 槽），上一行 = 目标缓冲上一行。
        debug_assert_eq!(out_row, srow);
        let zeros = vec![0u8; srow];
        loop {
            while y < height && ready >= (y + 1) * stride {
                let row_start = y * stride;
                let f = unsafe { shared.as_slice()[row_start] };
                let src = unsafe { &shared.as_slice()[row_start + 1..row_start + 1 + srow] };
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
                {
                    st.unfilter_ns += (c0.elapsed()).as_nanos() as u64;
                }
                y += 1;
            }
            if y == height {
                break;
            }
            if finalized {
                return Err(err_decode("IDAT 流过早结束"));
            }
            match rx.recv() {
                Ok(v) => ready = ready.max(v),
                Err(_) => {
                    // 生产者已结束：取终态（错误优先）。
                    if let Some(msg) = err_slot(shared).take() {
                        return Err(err_decode(msg));
                    }
                    ready = ready.max(shared.ready_load());
                    finalized = true;
                }
            }
        }
    } else {
        // 展开路径：2 槽 cur/above 轮流当上一行，unfilter_to 免拷贝。
        let mut cur = vec![0u8; srow];
        let mut above = vec![0u8; srow];
        loop {
            while y < height && ready >= (y + 1) * stride {
                let row_start = y * stride;
                let f = unsafe { shared.as_slice()[row_start] };
                let src = unsafe { &shared.as_slice()[row_start + 1..row_start + 1 + srow] };
                #[cfg(feature = "stage_profile")]
                let c0 = std::time::Instant::now();
                unfilter_to(f, src, &mut cur, &above, fbpp);
                #[cfg(feature = "stage_profile")]
                let c1 = std::time::Instant::now();
                let dst = &mut buf[y * out_row..y * out_row + out_row];
                crate::codecs::pngsimd::expand::expand_row(kind, table, &cur, dst, width);
                #[cfg(feature = "stage_profile")]
                {
                    st.unfilter_ns += (c0.elapsed()).as_nanos() as u64;
                    st.expand_ns += (c1.elapsed()).as_nanos() as u64;
                }
                std::mem::swap(&mut cur, &mut above);
                y += 1;
            }
            if y == height {
                break;
            }
            if finalized {
                return Err(err_decode("IDAT 流过早结束"));
            }
            match rx.recv() {
                Ok(v) => ready = ready.max(v),
                Err(_) => {
                    if let Some(msg) = err_slot(shared).take() {
                        return Err(err_decode(msg));
                    }
                    ready = ready.max(shared.ready_load());
                    finalized = true;
                }
            }
        }
    }

    Ok(())
}

/// 双核管道入口：起生产者线程并与消费者重叠（仅非隔行调用；`expected` = 每行 (filter+数据) 总和）。
pub(crate) fn decode_scanlines_pipelined(
    h: &FileHeader,
    data: &[u8],
    buf: &mut [u8],
    expected: usize,
    max_alloc: Option<u64>,
    st: &mut Stages,
) -> ImageResult<()> {
    let kind = build_expand_kind(h);
    let table = PaletteTable::from_header(h);
    let shared = InflateShared::new(expected);
    let (tx, rx) = sync_channel(4);

    #[cfg(feature = "stage_profile")]
    let t0 = std::time::Instant::now();

    let out = std::thread::scope(|s| {
        // tx 借入闭包后以值移入 → 生产者返回时 drop sender，通道断开（消费者 recv 才能收尾；
        // 共享作用域里再留一份 sender 会让出错路径的消费者永等 recv → 挂死）。shared 仅借用。
        let prod = s.spawn(|| inflate_producer(h, data, max_alloc, &shared, tx));
        let c = consume_rows(h, &shared, &rx, buf, &kind, &table, st);
        let _ = prod.join();
        // 错误收敛：消费者错误优先（其 recv 已 .take() 生产者错误）；消费者成功但生产者
        // 末尾才报错（长度不符/未正常结束等，消费者因行已齐而先返 Ok）以生产端为准。
        match c {
            Ok(()) => match err_slot(&shared).take() {
                Some(msg) => Err(err_decode(msg)),
                None => Ok(()),
            },
            Err(e) => Err(e),
        }
    });

    #[cfg(feature = "stage_profile")]
    {
        // 管道整体墙钟 ≈ inflate 阶段（解压 + 与行处理重叠的总时长）。
        st.inflate_ns += (t0.elapsed()).as_nanos() as u64;
    }
    out
}
