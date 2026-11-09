//! JPEG 段/表结构解析与解码器状态（1:1 复刻 zune-jpeg `headers.rs`/`misc.rs`/`components.rs`/
//! `decoder.rs::decode_headers_internal` 的解析语义）。

use core::cmp::max;
use std::time::Instant;

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::huffman::HuffmanTable;
use crate::codecs::jpeg::io::{ByteReader, Marker};
use crate::codecs::jpeg::upsampler::{upsample_no_op, UpSampler};

pub(crate) const MAX_COMPONENTS: usize = 4;
pub(crate) const DCT_BLOCK: usize = 64;

/// 阶段计时探针：`KIMG_JPEG_STAGE_PROFILE=1` 时在热路径累加各阶段耗时。
///
/// 两种时钟（本机单 vCPU VMware 实测，2026-11-05）：
/// - **墙钟**（`Instant::now()`）：绝对可信，但本机每次调用 ~61.6ns 且触发 VM exit；
///   只在**粗粒度**（整轮 decode、每 MCU 行、每行 post_process）使用——调用次数 ≤ 行数级，
///   开销可忽略。`rows/post/up/color/total/setup/header/mcu_rest` 走此路。
/// - **忙钟**（`_rdtsc()`）：~10.8ns/次，可逐块计；但 guest TSC 在内存停顿/宿主机争用期
///   不前进（墙钟≈RDTSC×7-10），内存密集型阶段（entropy 表查找、idct 负载）被系统性低估、
///   纯 ALU 循环骨架被相对放大（旧版 RDTSC 探针曾把 `inner` 伪占 75-90%）。故 entropy/idct
///   只作忙钟交叉参考，不据其定靶；权威 entropy/idct/loop 占比用「阶段消融 A/B」
///   （`KIMG_JPEG_SKIP_ENTROPY=1` / `KIMG_JPEG_SKIP_IDCT=1`，仅 profile 模式生效）以墙钟差值得出。
///
/// 默认关闭零开销：`timer()`/`stop()` 只做一次可预测分支，时钟对象仅启用时构造，
/// 不读 env、不构造时钟。env 判定进程级缓存，热环不重复读。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    Entropy,
    Idct,
    #[allow(dead_code)] // C# 语义换主后不再单独计时
    Upsample,
    #[allow(dead_code)]
    Color,
    Total,
    Header,
    Setup,
    DecodeRows,
    Post,
    McuRest,
}

/// 单次 `wall_timer()+stop_wall()` 的探针开销（2 次 `Instant::now()`，本机实测 61.6ns/次）。
/// 从墙钟阶段中扣除，避免把探针成本算进被测阶段。
const PROBE_WALL_NS: u64 = 123;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StageProfile {
    pub(crate) enabled: bool,
    /// 墙钟阶段累计（µs，f64 便于扣探针开销后的亚 µs 精度）。
    total_us: f64,
    header_us: f64,
    setup_us: f64,
    rows_us: f64,
    post_us: f64,
    upsample_us: f64,
    color_us: f64,
    mcu_rest_us: f64,
    /// 忙钟（rdtsc）阶段累计（µs）；仅供交叉参考，分布以消融 A/B 为准。
    entropy_us: f64,
    idct_us: f64,
    /// 调用计数（辅助解释 per-call 耗时时使用）。
    pub(crate) n_entropy: u64,
    pub(crate) n_idct: u64,
    pub(crate) n_rows: u64,
}

/// x86 TSC 计数 → ns 换算系数（进程内校准一次；非 x86 恒为 1ns/tick）。
/// 注意：校准循环写入易失变量防止编译器整体消除（曾因消除致 ns_per_tick 为垃圾值）。
fn ns_per_tick() -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        use core::sync::atomic::{compiler_fence, Ordering};
        use std::sync::OnceLock;
        static V: OnceLock<f64> = OnceLock::new();
        *V.get_or_init(|| {
            let t0 = unsafe { core::arch::x86_64::_rdtsc() };
            compiler_fence(Ordering::SeqCst);
            let s0 = std::time::Instant::now();
            let mut x: u64 = 0;
            for _ in 0..2_000_000 {
                x = x.wrapping_add(1);
            }
            unsafe {
                core::ptr::write_volatile(&mut x, x);
            }
            let s1 = std::time::Instant::now();
            compiler_fence(Ordering::SeqCst);
            let t1 = unsafe { core::arch::x86_64::_rdtsc() };
            let ns = s1.duration_since(s0).as_nanos().max(1) as f64;
            let ticks = t1.wrapping_sub(t0).max(1) as f64;
            ns / ticks
        })
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        1.0
    }
}

impl StageProfile {
    /// env 门控一次性判定（OnceLock 缓存，进程内只读一次）。
    pub(crate) fn enabled_by_env() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            std::env::var("KIMG_JPEG_STAGE_PROFILE")
                .map(|v| v.trim() == "1")
                .unwrap_or(false)
        })
    }

    /// 消融开关：profile 模式 + `KIMG_JPEG_SKIP_ENTROPY=1` 跳过 entropy 实际解码
    /// （`inner_decode_mcu_width` 直接填零 tmp），用于墙钟差值求 entropy 占比。
    pub(crate) fn skip_entropy() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            Self::enabled_by_env()
                && std::env::var("KIMG_JPEG_SKIP_ENTROPY")
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false)
        })
    }

    /// 消融开关：profile 模式 + `KIMG_JPEG_SKIP_IDCT=1` 跳过 IDCT 实际写回。
    pub(crate) fn skip_idct() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            Self::enabled_by_env()
                && std::env::var("KIMG_JPEG_SKIP_IDCT")
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false)
        })
    }

    /// 消融开关：profile 模式 + `KIMG_JPEG_SKIP_FILL=1` 跳过每块 `tmp.fill(0)`（骨架归因）。
    /// 注意：跳过清零后熵未写的间隙残留上一块残值，稀疏/全量 IDCT 读入垃圾 → 输出**错误**，
    /// 仅作墙钟归因测量用；字节恒等抽查必须回实路径（不设此 env）。
    pub(crate) fn skip_fill() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            Self::enabled_by_env()
                && std::env::var("KIMG_JPEG_SKIP_FILL")
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false)
        })
    }

    /// 墙钟计时（粗粒度调用点）。
    #[inline]
    pub(crate) fn timer(&self) -> Option<Instant> {
        if self.enabled {
            Some(Instant::now())
        } else {
            None
        }
    }

    /// 墙钟结算（粗粒度调用点）。
    #[inline]
    pub(crate) fn stop(&mut self, stage: Stage, t: Option<Instant>) {
        if let Some(t) = t {
            let ns = t.elapsed().as_nanos().saturating_sub(u128::from(PROBE_WALL_NS));
            let us = ns as f64 / 1000.0;
            match stage {
                Stage::Total => self.total_us += us,
                Stage::Header => self.header_us += us,
                Stage::Setup => self.setup_us += us,
                Stage::DecodeRows => self.rows_us += us,
                Stage::Post => self.post_us += us,
                Stage::Upsample => self.upsample_us += us,
                Stage::Color => self.color_us += us,
                Stage::McuRest => self.mcu_rest_us += us,
                // 逐块熵/IDCT 用忙钟，见 `busy_timer/busy_stop`。
                Stage::Entropy | Stage::Idct => {}
            }
        }
    }

    /// `KIMG_JPEG_NO_BUSY=1`：测量卫生开关（仅 profile 模式有意义）。置位时
    /// 忙钟（rdtsc）与 `n_entropy/n_idct` 逐块计数完全不注入被测窗口——窗口不变
    /// 但不再被插桩，得到无忙钟插桩的净值（消融 A/B / stage 分账用）。进程内一次读。
    pub(crate) fn no_busy() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            Self::enabled_by_env()
                && std::env::var("KIMG_JPEG_NO_BUSY")
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false)
        })
    }

    /// 逐块计数（`n_entropy/n_idct`）是否注入：与忙钟同门控（profile 开启且未设
    /// NO_BUSY）。production 关闭时恒 false——顺带去掉生产路径的无条件自增。
    #[inline]
    pub(crate) fn counting(&self) -> bool {
        self.enabled && !Self::no_busy()
    }

    /// 忙钟（rdtsc）计时——仅逐块熵/IDCT 交叉参考用，开销 ~10.8ns/次。
    /// NO_BUSY 置位时不注入（返回 None，busy_stop 随之短路），被测窗口不再被插桩。
    #[inline]
    pub(crate) fn busy_timer(&self) -> Option<u64> {
        if self.enabled && !Self::no_busy() {
            #[cfg(target_arch = "x86_64")]
            {
                Some(unsafe { core::arch::x86_64::_rdtsc() })
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                Some(0)
            }
        } else {
            None
        }
    }

    /// 忙钟结算——逐块熵/IDCT。
    #[inline]
    pub(crate) fn busy_stop(&mut self, stage: Stage, t: Option<u64>) {
        if let Some(t) = t {
            #[cfg(not(target_arch = "x86_64"))]
            let _ = t;
            #[cfg(target_arch = "x86_64")]
            {
                let us =
                    unsafe { core::arch::x86_64::_rdtsc().wrapping_sub(t) } as f64
                        * ns_per_tick()
                        / 1000.0;
                match stage {
                    Stage::Entropy => self.entropy_us += us,
                    Stage::Idct => self.idct_us += us,
                    _ => {}
                }
            }
        }
    }

    /// 打印本 decode 的阶段分布（只读 side-effect，env 门控）。
    pub(crate) fn print(&self) {
        if !self.enabled {
            return;
        }
        // 占比分母 = header + mcu 解码全时段（墙钟）
        let full = (self.header_us + self.total_us).max(1.0);
        let pct = |us: f64| us / full * 100.0;
        let other = (self.total_us - self.setup_us - self.rows_us - self.post_us).max(0.0);
        println!(
            "STAGE-DECODE header={:.3}ms mcu_total={:.3}ms rows={:.3}ms x{}[nidct={}] loop=(ent(busy){:.3}+idct(busy){:.3})+mcuRest{:.3} post={:.3}ms[up={:.3}+color={:.3}] other={:.3}ms({:.1}%) | entropy/idct 真值看消融 A/B",
            self.header_us / 1000.0,
            self.total_us / 1000.0,
            self.rows_us / 1000.0,
            self.n_rows,
            self.n_idct,
            self.entropy_us / 1000.0,
            self.idct_us / 1000.0,
            self.mcu_rest_us / 1000.0,
            self.post_us / 1000.0,
            self.upsample_us / 1000.0,
            self.color_us / 1000.0,
            other / 1000.0,
            pct(other),
        );
    }
}

/// 输入/输出颜色空间（zune-core `ColorSpace` 的子集；部分变体仅用于镜像映射）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ColorSpace {
    YCbCr,
    RGB,
    RGBA,
    Luma,
    LumaA,
    CMYK,
    YCCK,
    MultiBand(usize),
}

impl ColorSpace {
    pub(crate) fn num_components(self) -> usize {
        match self {
            ColorSpace::YCbCr | ColorSpace::RGB => 3,
            ColorSpace::RGBA => 4,
            ColorSpace::Luma => 1,
            ColorSpace::LumaA => 2,
            ColorSpace::CMYK | ColorSpace::YCCK => 4,
            ColorSpace::MultiBand(n) => n,
        }
    }
}

/// 采样比例。
#[derive(Copy, Debug, Clone, PartialEq, Eq)]
pub(crate) enum SampleRatios {
    HV,
    V,
    H,
    Generic(usize, usize),
    None,
}

impl SampleRatios {
    pub(crate) fn sample(self) -> usize {
        match self {
            SampleRatios::HV => 4,
            SampleRatios::V | SampleRatios::H => 2,
            SampleRatios::Generic(a, b) => a * b,
            SampleRatios::None => 1,
        }
    }
}

/// 图像分量（含解码所需的平面缓冲）。
#[derive(Clone)]
pub(crate) struct Component {
    /// 原始分量 id（SOF 首字节）。
    pub(crate) id: u8,
    pub(crate) vertical_sample: usize,
    pub(crate) horizontal_sample: usize,
    pub(crate) dc_huff_table: usize,
    pub(crate) ac_huff_table: usize,
    pub(crate) quantization_table_number: u8,
    pub(crate) quantization_table: [i32; 64],
    /// C# `ZigQ` 融合表（`build_zigzag_quant` 由 natural-order 量化表派生，基线 AC 反量化单装载）。
    pub(crate) quantization_zq: [u32; ZIGZAG_QUANT_LEN],
    pub(crate) dc_pred: i32,
    pub(crate) up_sampler: UpSampler,
    /// 行宽（水平采样 × mcu_x × 8）。
    pub(crate) width_stride: usize,
    pub(crate) needed: bool,
    pub(crate) raw_coeff: Vec<i16>,
    pub(crate) upsample_dest: Vec<i16>,
    pub(crate) row_up: Vec<i16>,
    pub(crate) row: Vec<i16>,
    pub(crate) first_row_upsample_dest: Vec<i16>,
    pub(crate) sample_ratio: SampleRatios,
    pub(crate) fix_an_annoying_bug: usize,
    // ---- C# 数值核心状态（`cs.rs`；zune 上采样/颜色两套并行，csharp 路径用此） ----
    /// 双 MCU 行样本环（`RingRows × W2` 字节采样平面；RingOffset(row) = row%RingRows*W2）。
    pub(crate) cs_ring: Vec<u8>,
    /// 平面行宽（= `width_stride`）。
    pub(crate) cs_w2: usize,
    /// 水平 / 垂直采样倍率（`Hs = hMax/H`、`Vs = vMax/V`）。
    pub(crate) cs_hs: usize,
    pub(crate) cs_vs: usize,
    /// 低分辨率样本数 `WLores = ceil(OutX / Hs)`。
    pub(crate) cs_wlores: usize,
    /// 平面高 `YsScaled = ceil(comp.Y / scale)`（scale=1）。
    pub(crate) cs_ys_scaled: usize,
    /// 每 MCU 行贡献的样本行数 `V*8` / 环总行数 `2*V*8`。
    pub(crate) cs_rows_mcu: usize,
    pub(crate) cs_ring_rows: usize,
    /// 垂直上采样位置状态（C# `Line0/Line1/YStep/YPos`）。
    pub(crate) cs_line0: usize,
    pub(crate) cs_line1: usize,
    pub(crate) cs_ystep: usize,
    pub(crate) cs_ypos: usize,
    /// 已解码平面行数（每 MCU 行提交后 = (m+1)*RowsPerMcu）。
    pub(crate) cs_rows_avail: usize,
    /// `JpegResample` 核类型（`cs::R_*`）。
    pub(crate) cs_resample_kind: u8,
    /// 8 位 / 16× 上采样行缓冲（`OutX+3` 容量）。
    pub(crate) cs_line_buf8: Vec<u8>,
    pub(crate) cs_line_buf16: Vec<u16>,
    /// 本输出行的源：`cs_row_is_ring=true` → `cs_ring[cs_row_near..]`；否则 `cs_line_buf8`。
    pub(crate) cs_row_is_ring: bool,
    pub(crate) cs_row_near: usize,
    /// 本输出行的 16× 色度是否已在 `cs_line_buf16`（供颜色核分辨色度 16/8 位/环）。
    pub(crate) cs_chroma_ready: bool,
}

impl Component {
    /// 由 SOF 中每分量 3 字节构造（pos 为分量序号）。
    pub(crate) fn from(a: [u8; 3], pos: usize) -> Result<Component, DecodeError> {
        if pos >= MAX_COMPONENTS {
            return Err(DecodeError::Format(format!(
                "Unknown component id found,{pos}, expected value between 1 and 4"
            )));
        }

        let horizontal_sample = ((a[1] >> 4) & 0x0F) as usize;
        let vertical_sample = (a[1] & 0x0f) as usize;

        if horizontal_sample > 4 {
            return Err(DecodeError::Format(format!(
                "Bogus Horizontal Sampling Factor {horizontal_sample}"
            )));
        }
        if vertical_sample > 4 {
            return Err(DecodeError::Format(format!(
                "Bogus Vertical Sampling Factor {vertical_sample}"
            )));
        }

        let quantization_table_number = a[2];
        if usize::from(quantization_table_number) >= MAX_COMPONENTS {
            return Err(DecodeError::Format(format!(
                "Too large quantization number :{quantization_table_number}, expected value between 0 and {MAX_COMPONENTS}"
            )));
        }
        if !horizontal_sample.is_power_of_two() {
            return Err(DecodeError::Format(format!(
                "Horizontal sample is not a power of two({horizontal_sample}) cannot decode"
            )));
        }
        if vertical_sample == 0 {
            return Err(DecodeError::Format("Vertical sample is zero".to_string()));
        }

        Ok(Component {
            id: a[0],
            vertical_sample,
            horizontal_sample,
            quantization_table_number,
            dc_huff_table: 0,
            ac_huff_table: 0,
            quantization_table: [0; 64],
            quantization_zq: [0; ZIGZAG_QUANT_LEN],
            dc_pred: 0,
            up_sampler: upsample_no_op,
            width_stride: horizontal_sample,
            needed: true,
            raw_coeff: vec![],
            upsample_dest: vec![],
            row_up: vec![],
            row: vec![],
            first_row_upsample_dest: vec![],
            sample_ratio: SampleRatios::None,
            fix_an_annoying_bug: 1,
            cs_ring: vec![],
            cs_w2: 0,
            cs_hs: 1,
            cs_vs: 1,
            cs_wlores: 0,
            cs_ys_scaled: 0,
            cs_rows_mcu: 0,
            cs_ring_rows: 0,
            cs_line0: 0,
            cs_line1: 0,
            cs_ystep: 0,
            cs_ypos: 0,
            cs_rows_avail: 0,
            cs_resample_kind: crate::codecs::jpeg::cs::R_ONE_TO_ONE,
            cs_line_buf8: vec![],
            cs_line_buf16: vec![],
            cs_row_is_ring: true,
            cs_row_near: 0,
            cs_chroma_ready: false,
        })
    }

    /// 上采样工作缓冲分配已移除（dormancy）：C# 语义换主后 `upsampler::upsample` 不再被
    /// 管线调用，`row/row_up/first_row_upsample_dest/upsample_dest` 四处分配纯死
    /// （2026-11-08 删），字段保留为空 Vec 以维持 `upsampler.rs`（休眠模块）编译。

    /// C# `RingOffset`: 平面行 → 环字节偏移（`row % RingRows * W2`）。
    #[inline]
    pub(crate) fn cs_ring_offset(&self, row: usize) -> usize {
        (row % self.cs_ring_rows) * self.cs_w2
    }

    /// C# `AdvanceLine`: 每输出行步进垂直游标。
    #[inline]
    pub(crate) fn cs_advance_line(&mut self) {
        self.cs_ystep += 1;
        if self.cs_ystep >= self.cs_vs {
            self.cs_ystep = 0;
            self.cs_line0 = self.cs_line1;
            self.cs_ypos += 1;
            if self.cs_ypos < self.cs_ys_scaled {
                self.cs_line1 += 1;
            }
        }
    }
}

/// ICC 分块（去 APP2 前缀后按序号重组）。
pub(crate) struct ICCChunk {
    pub(crate) seq_no: u8,
    pub(crate) num_markers: u8,
    pub(crate) data: Vec<u8>,
}

/// 解码器状态（既是段解析器也是 MCU 解码器）。
pub(crate) struct Decoder<'a> {
    pub(crate) reader: ByteReader<'a>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) components: Vec<Component>,
    pub(crate) qt_tables: [Option<[i32; DCT_BLOCK]>; MAX_COMPONENTS],
    pub(crate) dc_huffman_tables: [Option<HuffmanTable>; MAX_COMPONENTS],
    pub(crate) ac_huffman_tables: [Option<HuffmanTable>; MAX_COMPONENTS],
    pub(crate) h_max: usize,
    pub(crate) v_max: usize,
    pub(crate) mcu_width: usize,
    pub(crate) mcu_height: usize,
    pub(crate) mcu_x: usize,
    pub(crate) mcu_y: usize,
    pub(crate) is_interleaved: bool,
    pub(crate) input_colorspace: ColorSpace,
    pub(crate) out_colorspace: ColorSpace,
    pub(crate) is_progressive: bool,
    pub(crate) spec_start: u8,
    pub(crate) spec_end: u8,
    pub(crate) succ_high: u8,
    pub(crate) succ_low: u8,
    pub(crate) num_scans: u8,
    pub(crate) scan_subsampled: bool,
    pub(crate) z_order: [usize; MAX_COMPONENTS],
    pub(crate) restart_interval: usize,
    pub(crate) todo: usize,
    /// 展示降档分母（=Some(2) 时按 den=2 DCT 缩放解码，2026-11 新增；None = 全解）。
    /// 仅解码级状态（不入 HeaderState）：`read_image_scaled` 在恢复头部后设置。
    pub(crate) scaled_den: Option<u8>,
    pub(crate) headers_decoded: bool,
    pub(crate) seen_sof: bool,
    pub(crate) icc_data: Vec<ICCChunk>,
    pub(crate) coeff: usize,
    pub(crate) sample_ratio: SampleRatios,
    pub(crate) exif_data: Option<Vec<u8>>,
    pub(crate) xmp_data: Option<Vec<u8>>,
    pub(crate) iptc_data: Option<Vec<u8>>,
    // ---- C# 数值核心（`cs.rs`）解码级状态 ----
    /// 已写出的输出行（从 0 单调递增到 `height`）。
    pub(crate) cs_out_row: usize,
    /// 色度是否走 16×（`_chroma16`）。
    pub(crate) cs_chroma16: bool,
    /// 3 分量输入是否 RGB 编码（`_isRgb`）。
    pub(crate) cs_is_rgb_input: bool,
    /// 输入颜色空间类别（选择输出行核）。
    pub(crate) cs_input_kind: crate::codecs::jpeg::cs::CsInput,
    /// 本次解码是否已初始化 cs 环（避免重复初始化）。
    pub(crate) cs_inited: bool,
    /// 阶段计时探针（默认关闭，零开销）。
    pub(crate) stage: StageProfile,
}

    /// 头部解析后可复用的解码状态（C2：省 `read_image` 第二遍 header 解析）。
    ///
    /// `decode_headers_internal` 返回后一次性从 `Decoder` 提取（move 拥有态字段），
    /// 之后 `restore_header_state` 可把它等价装回一个借用同一输入的新 `Decoder`
    /// （reader 对准 `scan_start` = 熵编码数据起点，`headers_decoded=true`，
    /// `decode_baseline/progressive` 内部二次 `decode_headers_internal` 随之短路）。
    /// 字段集与 `Decoder` 中「段解析建立、解码路径消费」的部分一一对应；两边同时
    /// 维护的不变量由 `jpeg_csharp_parity` 全量字节对照兜底。
    /// Huffman 表装箱（`Box<HuffmanTable>`）：每表 ~20KB，沿 `HeaderState` 逐值搬运
    /// 会在 debug 1MB 栈上多帧累计爆栈（K82），装箱后结构体 ~1.2KB。
    pub(crate) struct HeaderState {
        pub(crate) width: usize,
        pub(crate) height: usize,
        pub(crate) components: Vec<Component>,
        pub(crate) qt_tables: [Option<[i32; DCT_BLOCK]>; MAX_COMPONENTS],
        pub(crate) dc_huffman_tables: [Option<Box<HuffmanTable>>; MAX_COMPONENTS],
        pub(crate) ac_huffman_tables: [Option<Box<HuffmanTable>>; MAX_COMPONENTS],
        pub(crate) h_max: usize,
        pub(crate) v_max: usize,
        pub(crate) is_interleaved: bool,
        pub(crate) input_colorspace: ColorSpace,
        pub(crate) is_progressive: bool,
        pub(crate) spec_start: u8,
        pub(crate) spec_end: u8,
        pub(crate) succ_high: u8,
        pub(crate) succ_low: u8,
        pub(crate) num_scans: u8,
        pub(crate) scan_subsampled: bool,
        pub(crate) z_order: [usize; MAX_COMPONENTS],
        pub(crate) restart_interval: usize,
        pub(crate) todo: usize,
        pub(crate) coeff: usize,
        pub(crate) sample_ratio: SampleRatios,
        /// 头部解析结束（= 首个 SOS 段末）的 reader 绝对位置 = 熵编码数据起点。
        pub(crate) scan_start: usize,
    }

    impl<'a> Decoder<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Decoder<'a> {
        Decoder {
            reader: ByteReader::new(data),
            width: 0,
            height: 0,
            components: vec![],
            qt_tables: [None; MAX_COMPONENTS],
            dc_huffman_tables: core::array::from_fn(|_| None),
            ac_huffman_tables: core::array::from_fn(|_| None),
            h_max: 1,
            v_max: 1,
            mcu_width: 0,
            mcu_height: 0,
            mcu_x: 0,
            mcu_y: 0,
            is_interleaved: false,
            input_colorspace: ColorSpace::YCbCr,
            out_colorspace: ColorSpace::RGB,
            is_progressive: false,
            spec_start: 0,
            spec_end: 0,
            succ_high: 0,
            succ_low: 0,
            num_scans: 0,
            scan_subsampled: false,
            z_order: [0; MAX_COMPONENTS],
            restart_interval: 0,
            todo: 0x7fff_ffff,
            scaled_den: None,
            headers_decoded: false,
            seen_sof: false,
            icc_data: vec![],
            coeff: 1,
            sample_ratio: SampleRatios::None,
            exif_data: None,
            xmp_data: None,
            iptc_data: None,
            cs_out_row: 0,
            cs_chroma16: false,
            cs_is_rgb_input: false,
            cs_input_kind: crate::codecs::jpeg::cs::CsInput::Ycc,
            cs_inited: false,
            stage: StageProfile {
                enabled: StageProfile::enabled_by_env(),
                ..Default::default()
            },
        }
    }

    /// 解析段直到首个 SOS（等价 zune `decode_headers_internal`）。
    pub(crate) fn decode_headers_internal(&mut self) -> Result<(), DecodeError> {
        if self.headers_decoded {
            return Ok(());
        }

        let magic_bytes = self.reader.get_u16_be_err()?;

        let mut last_byte = 0;
        let mut bytes_before_marker = 0;

        if magic_bytes != 0xffd8 {
            return Err(DecodeError::Format(format!(
                "Error parsing image. Illegal start bytes:{magic_bytes:X}"
            )));
        }

        loop {
            let mut m = self.reader.read_u8_err()?;

            // 处理 marker 间的填充字节（0xFF）与字节填充（0）
            if (m == 0xFF || m == 0) && last_byte == 0xFF {
                while m == 0xFF || m == 0x0 {
                    last_byte = m;
                    m = self.reader.read_u8_err()?;
                }
            }
            if last_byte == 0xFF {
                let marker = Marker::from_u8(m);
                if let Some(n) = marker {
                    if bytes_before_marker > 3 {
                        // strict_mode=false：忽略“多出来的字节”
                    }
                    bytes_before_marker = 0;

                    self.parse_marker_inner(n)?;

                    if n == Marker::SOS {
                        self.headers_decoded = true;

                        // jpegli 等以 'R','G','B' 为分量 id 的 RGB 图像
                        let mut is_rgb = self.components.len() == 3;
                        let chars = ['R', 'G', 'B'];
                        for (comp, single_char) in self.components.iter().zip(chars.iter()) {
                            is_rgb &= comp.id == (*single_char) as u8;
                        }
                        if is_rgb {
                            self.input_colorspace = ColorSpace::RGB;
                        }

                        return Ok(());
                    }
                } else {
                    bytes_before_marker = 0;
                    let length = self.reader.get_u16_be_err()?;
                    if length < 2 {
                        return Err(DecodeError::Format(format!(
                            "Found a marker with invalid length : {length}"
                        )));
                    }
                    self.reader.skip((length - 2) as usize)?;
                }
            }
            last_byte = m;
            bytes_before_marker += 1;
        }
    }

    /// 提取头部解析后的解码状态（C2）。move 拥有态字段（components/huffman/量化表），
    /// 并记录 reader 在熵编码数据起点的绝对位置。调用方须在提取后再取 metadata。
    pub(crate) fn extract_header_state(&mut self) -> HeaderState {
        HeaderState {
            width: self.width,
            height: self.height,
            components: core::mem::take(&mut self.components),
            qt_tables: core::mem::take(&mut self.qt_tables),
            dc_huffman_tables: core::mem::take(&mut self.dc_huffman_tables)
                .map(|o| o.map(Box::new)),
            ac_huffman_tables: core::mem::take(&mut self.ac_huffman_tables)
                .map(|o| o.map(Box::new)),
            h_max: self.h_max,
            v_max: self.v_max,
            is_interleaved: self.is_interleaved,
            input_colorspace: self.input_colorspace,
            is_progressive: self.is_progressive,
            spec_start: self.spec_start,
            spec_end: self.spec_end,
            succ_high: self.succ_high,
            succ_low: self.succ_low,
            num_scans: self.num_scans,
            scan_subsampled: self.scan_subsampled,
            z_order: self.z_order,
            restart_interval: self.restart_interval,
            todo: self.todo,
            coeff: self.coeff,
            sample_ratio: self.sample_ratio,
            scan_start: self.reader.position(),
        }
    }

    /// 把 `extract_header_state` 的结果装回一个借用同一输入的新 `Decoder`
    /// （reader 对准熵编码数据起点、`headers_decoded=true`），省去第二遍 header 解析。
    pub(crate) fn restore_header_state(&mut self, s: HeaderState) {
        self.width = s.width;
        self.height = s.height;
        self.components = s.components;
        self.qt_tables = s.qt_tables;
        // 解箱回 Decoder 的内联表数组（一次只解一张，~20KB 栈瞬时，避免多帧累计）。
        self.dc_huffman_tables = core::array::from_fn(|_| None);
        for (dst, src) in self
            .dc_huffman_tables
            .iter_mut()
            .zip(s.dc_huffman_tables.into_iter())
        {
            *dst = src.map(|b| *b);
        }
        self.ac_huffman_tables = core::array::from_fn(|_| None);
        for (dst, src) in self
            .ac_huffman_tables
            .iter_mut()
            .zip(s.ac_huffman_tables.into_iter())
        {
            *dst = src.map(|b| *b);
        }
        self.h_max = s.h_max;
        self.v_max = s.v_max;
        self.is_interleaved = s.is_interleaved;
        self.input_colorspace = s.input_colorspace;
        self.is_progressive = s.is_progressive;
        self.spec_start = s.spec_start;
        self.spec_end = s.spec_end;
        self.succ_high = s.succ_high;
        self.succ_low = s.succ_low;
        self.num_scans = s.num_scans;
        self.scan_subsampled = s.scan_subsampled;
        self.z_order = s.z_order;
        self.restart_interval = s.restart_interval;
        self.todo = s.todo;
        self.coeff = s.coeff;
        self.sample_ratio = s.sample_ratio;
        self.headers_decoded = true;
        self.seen_sof = true;
        self.reader.set_position(s.scan_start);
    }

    /// 统一 marker 分发（等价 zune `parse_marker_inner`）。
    pub(crate) fn parse_marker_inner(&mut self, m: Marker) -> Result<(), DecodeError> {
        match m {
            Marker::SOF(0..=2) => {
                if m == Marker::SOF(2) {
                    // SOF2 progressive：标记渐进帧，随后按 zune 的解码流程处理
                    self.is_progressive = true;
                }
                self.parse_start_of_frame()?;
            }
            Marker::SOF(v) => {
                if let Some(feature) = unsupported_scheme_desc(v) {
                    return Err(DecodeError::Unsupported(feature));
                }
                return Err(DecodeError::Format("Unsupported image format".to_string()));
            }
            Marker::APP(0) => {
                let mut length = self.reader.get_u16_be_err()?;
                if length < 2 {
                    return Err(DecodeError::Format(format!(
                        "Found a marker with invalid length:{length}\n"
                    )));
                }
                // AVI1 检为 MJPEG（本实现不做，仅按长度跳过）
                if length > 5 {
                    let mut buffer = [0u8; 5];
                    self.reader.read_exact_bytes(&mut buffer)?;
                    let _ = buffer;
                    length -= 5;
                }
                self.reader.skip(length.saturating_sub(2) as usize)?;
            }
            Marker::APP(1) => {
                self.parse_app1()?;
            }
            Marker::APP(2) => {
                self.parse_app2()?;
            }
            Marker::DQT => {
                self.parse_dqt()?;
            }
            Marker::DHT => {
                self.parse_huffman()?;
            }
            Marker::SOS => {
                self.parse_sos()?;
            }
            Marker::EOI => {
                return Err(DecodeError::Format("Premature End of image".to_string()));
            }
            Marker::DAC | Marker::DNL => {
                return Err(DecodeError::Format(format!(
                    "Parsing of the following header `{m:?}` is not supported,cannot continue"
                )));
            }
            Marker::DRI => {
                if self.reader.get_u16_be_err()? != 4 {
                    return Err(DecodeError::Format(
                        "Bad DRI length, Corrupt JPEG".to_string(),
                    ));
                }
                self.restart_interval = usize::from(self.reader.get_u16_be_err()?);
                self.todo = self.restart_interval;
            }
            Marker::APP(14) => {
                self.parse_app14()?;
            }
            Marker::APP(13) => {
                self.parse_app13()?;
            }
            _ => {
                let length = self.reader.get_u16_be_err()?;
                if length < 2 {
                    return Err(DecodeError::Format(format!(
                        "Found a marker with invalid length:{length}\n"
                    )));
                }
                self.reader.skip((length - 2) as usize)?;
            }
        }
        Ok(())
    }

    /// 解析 Huffman 表段（DHT）。
    fn parse_huffman(&mut self) -> Result<(), DecodeError> {
        let mut dht_length = i32::from(
            self.reader
                .get_u16_be_err()?
                .checked_sub(2)
                .ok_or(DecodeError::Format("Invalid Huffman length in image".to_string()))?,
        );

        while dht_length > 16 {
            let ht_info = self.reader.read_u8_err()?;
            let dc_or_ac = (ht_info >> 4) & 0xF;
            let index = (ht_info & 0xF) as usize;
            let mut num_symbols: [u8; 17] = [0; 17];

            if index >= MAX_COMPONENTS {
                return Err(DecodeError::HuffmanDecode(format!(
                    "Invalid DHT index {index}, expected between 0 and 3"
                )));
            }
            if dc_or_ac > 1 {
                return Err(DecodeError::HuffmanDecode(format!(
                    "Invalid DHT position {dc_or_ac}, should be 0 or 1"
                )));
            }

            self.reader.read_exact_bytes(&mut num_symbols[1..17])?;

            dht_length -= 1 + 16;

            let symbols_sum: i32 = num_symbols.iter().map(|f| i32::from(*f)).sum();

            if symbols_sum > 256 {
                return Err(DecodeError::Format(
                    "Encountered Huffman table with excessive length in DHT".to_string(),
                ));
            }
            if symbols_sum > dht_length {
                return Err(DecodeError::HuffmanDecode(format!(
                    "Excessive Huffman table of length {symbols_sum} found when header length is {dht_length}"
                )));
            }
            dht_length -= symbols_sum;
            let mut symbols = [0; 256];

            self.reader
                .read_exact_bytes(&mut symbols[0..(symbols_sum as usize)])?;
            match dc_or_ac {
                0 => {
                    self.dc_huffman_tables[index] =
                        Some(HuffmanTable::new(&num_symbols, symbols, true, self.is_progressive)?);
                }
                _ => {
                    self.ac_huffman_tables[index] =
                        Some(HuffmanTable::new(&num_symbols, symbols, false, self.is_progressive)?);
                }
            }
        }

        if dht_length > 0 {
            return Err(DecodeError::Format("Bogus Huffman table definition".to_string()));
        }

        Ok(())
    }

    /// 解析量化表段（DQT）。
    fn parse_dqt(&mut self) -> Result<(), DecodeError> {
        let mut qt_length = self
            .reader
            .get_u16_be_err()?
            .checked_sub(2)
            .ok_or(DecodeError::Format("Invalid DQT length. Length should be greater than 2".to_string()))?;

        while qt_length > 0 {
            let qt_info = self.reader.read_u8_err()?;
            let precision = (qt_info >> 4) as usize;
            let table_position = (qt_info & 0x0f) as usize;
            let precision_value = 64 * (precision + 1);

            if (precision_value + 1) as u16 > qt_length {
                return Err(DecodeError::DqtError(format!(
                    "Invalid QT table bytes left :{qt_length}. Too small to construct a valid qt table which should be {} long",
                    precision_value + 1
                )));
            }

            let dct_table = match precision {
                0 => {
                    let mut qt_values = [0; 64];
                    self.reader.read_exact_bytes(&mut qt_values)?;
                    qt_length -= (precision_value as u16) + 1;
                    un_zig_zag(&qt_values)
                }
                1 => {
                    let mut qt_values = [0_u16; 64];
                    for i in 0..64 {
                        qt_values[i] = self.reader.get_u16_be_err()?;
                    }
                    qt_length -= (precision_value as u16) + 1;
                    un_zig_zag(&qt_values)
                }
                _ => {
                    return Err(DecodeError::DqtError(format!(
                        "Expected QT precision value of either 0 or 1, found {precision:?}"
                    )));
                }
            };

            if table_position >= MAX_COMPONENTS {
                return Err(DecodeError::DqtError(format!(
                    "Too large table position for QT :{table_position}, expected between 0 and 3"
                )));
            }

            self.qt_tables[table_position] = Some(dct_table);
        }

        Ok(())
    }

    /// 解析帧头（SOF）。
    fn parse_start_of_frame(&mut self) -> Result<(), DecodeError> {
        if self.seen_sof {
            return Err(DecodeError::SofError(
                "Two Start of Frame Markers".to_string(),
            ));
        }
        let length = self.reader.get_u16_be_err()?;
        let dt_precision = self.reader.read_u8_err()?;

        if dt_precision != 8 {
            return Err(DecodeError::SofError(format!(
                "The library can only parse 8-bit images, the image has {dt_precision} bits of precision"
            )));
        }

        let img_height = u32::from(self.reader.get_u16_be_err()?);
        let img_width = u32::from(self.reader.get_u16_be_err()?);

        if img_width == 0 || img_height == 0 {
            return Err(DecodeError::SofError(
                "Image width or height is set to zero, cannot continue".to_string(),
            ));
        }

        let num_components = self.reader.read_u8_err()?;

        if num_components == 0 {
            return Err(DecodeError::SofError(
                "Number of components cannot be zero.".to_string(),
            ));
        }

        let expected = 8 + 3 * u16::from(num_components);
        if length != expected {
            return Err(DecodeError::SofError(format!(
                "Length of start of frame differs from expected {expected},value is {length}"
            )));
        }

        if num_components == 1 {
            self.input_colorspace = ColorSpace::Luma;
        }
        if num_components == 4 && self.input_colorspace == ColorSpace::YCbCr {
            self.input_colorspace = ColorSpace::CMYK;
        }

        let mut components = Vec::with_capacity(num_components as usize);
        let mut temp = [0; 3];

        for pos in 0..num_components {
            self.reader.read_exact_bytes(&mut temp)?;
            let component = Component::from(temp, pos as usize)?;
            components.push(component);
        }
        self.seen_sof = true;

        self.components = components;
        self.width = img_width as usize;
        self.height = img_height as usize;

        let mut h_max = 1;
        let mut v_max = 1;

        for comp in &self.components {
            h_max = max(h_max, comp.horizontal_sample);
            v_max = max(v_max, comp.vertical_sample);
        }

        self.sample_ratio = match (h_max, v_max) {
            (1, 1) => SampleRatios::None,
            (1, 2) => SampleRatios::V,
            (2, 1) => SampleRatios::H,
            (2, 2) => SampleRatios::HV,
            (hs, vs) => SampleRatios::Generic(hs, vs),
        };

        Ok(())
    }

    /// 解析扫描头（SOS）。
    fn parse_sos(&mut self) -> Result<(), DecodeError> {
        let ls = usize::from(self.reader.get_u16_be_err()?);
        let ns = self.reader.read_u8_err()?;

        let mut seen: [_; MAX_COMPONENTS + 1] = [-1; MAX_COMPONENTS + 1];

        self.num_scans = ns;
        let smallest_size = 6 + 2 * usize::from(ns);

        if ls != smallest_size {
            return Err(DecodeError::SosError(format!(
                "Bad SOS length {ls},corrupt jpeg"
            )));
        }
        if !(1..5).contains(&ns) {
            return Err(DecodeError::SosError(format!(
                "Invalid number of components in start of scan {ns}, expected in range 1..5"
            )));
        }
        if self.components.is_empty() {
            return Err(DecodeError::Format(
                "Error decoding SOF Marker, Number of components cannot be zero.".to_string(),
            ));
        }

        self.scan_subsampled = false;

        for i in 0..ns {
            let id = self.reader.read_u8_err()?;

            if seen.contains(&i32::from(id)) {
                return Err(DecodeError::SofError(format!(
                    "Duplicate ID {id} seen twice in the same component"
                )));
            }

            seen[usize::from(i)] = i32::from(id);
            let y = self.reader.read_u8_err()?;

            let mut j = 0;
            while j < self.components.len() {
                if self.components[j].id == id {
                    break;
                }
                j += 1;
            }

            if j == self.components.len() {
                return Err(DecodeError::SofError(format!(
                    "Invalid component id {}, expected one of {:?}",
                    id,
                    self.components.iter().map(|c| c.id).collect::<Vec<_>>()
                )));
            }

            let component = &mut self.components[j];
            component.dc_huff_table = usize::from((y >> 4) & 0xF);
            component.ac_huff_table = usize::from(y & 0xF);
            self.z_order[usize::from(i)] = j;

            if component.vertical_sample != 1 || component.horizontal_sample != 1 {
                self.scan_subsampled = true;
            }
        }

        self.spec_start = self.reader.read_u8_err()?;
        self.spec_end = self.reader.read_u8_err()?;

        let bit_approx = self.reader.read_u8_err()?;
        self.succ_high = bit_approx >> 4;

        if self.spec_end > 63 {
            return Err(DecodeError::SosError(format!(
                "Invalid Se parameter {}, range should be 0-63",
                self.spec_end
            )));
        }
        if self.spec_start > 63 {
            return Err(DecodeError::SosError(format!(
                "Invalid Ss parameter {}, range should be 0-63",
                self.spec_start
            )));
        }
        if self.succ_high > 13 {
            return Err(DecodeError::SosError(format!(
                "Invalid Ah parameter {}, range should be 0-13",
                self.succ_high
            )));
        }
        self.succ_low = bit_approx & 0xF;

        if self.succ_low > 13 {
            return Err(DecodeError::SosError(format!(
                "Invalid Al parameter {}, range should be 0-13",
                self.succ_low
            )));
        }
        self.reader.skip(smallest_size.saturating_sub(ls))?;

        Ok(())
    }

    /// 解析 APP13（IPTC）。
    fn parse_app13(&mut self) -> Result<(), DecodeError> {
        const IPTC_PREFIX: &[u8] = b"Photoshop 3.0\0";
        let mut length = usize::from(self.reader.get_u16_be());

        if length < 2 {
            return Err(DecodeError::Format("Too small APP13 length".to_string()));
        }
        length -= 2;

        if length > IPTC_PREFIX.len()
            && self.reader.peek_at(0, IPTC_PREFIX.len())? == IPTC_PREFIX
        {
            self.reader.skip(IPTC_PREFIX.len())?;
            length -= IPTC_PREFIX.len();

            let iptc_bytes = self.reader.peek_at(0, length)?.to_vec();
            self.iptc_data = Some(iptc_bytes);
        }

        self.reader.skip(length)?;
        Ok(())
    }

    /// 解析 APP14（Adobe）。
    fn parse_app14(&mut self) -> Result<(), DecodeError> {
        let mut length = usize::from(self.reader.get_u16_be());

        if length < 2 {
            return Err(DecodeError::Format("Too small APP14 length".to_string()));
        }

        if self.reader.peek_at(0, 5)? == b"Adobe" {
            if length < 14 {
                return Err(DecodeError::Format(
                    "Too short of a length for App14 segment".to_string(),
                ));
            }
            self.reader.skip(6)?;
            self.reader.skip(5)?;
            let transform = self.reader.read_u8();
            match transform {
                0 => self.input_colorspace = ColorSpace::CMYK,
                1 => self.input_colorspace = ColorSpace::YCbCr,
                2 => self.input_colorspace = ColorSpace::YCCK,
                _ => {
                    return Err(DecodeError::Format(format!(
                        "Unknown Adobe colorspace {transform}"
                    )))
                }
            }
            length = length.saturating_sub(14);
        } else {
            length = length.saturating_sub(2);
        }
        self.reader.skip(length)?;

        Ok(())
    }

    /// 解析 APP1（EXIF/XMP）。
    fn parse_app1(&mut self) -> Result<(), DecodeError> {
        const XMP_NAMESPACE_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

        let mut length = usize::from(self.reader.get_u16_be());

        if length < 2 {
            return Err(DecodeError::Format("Too small app1 length".to_string()));
        }
        length -= 2;

        if length > 6 && self.reader.peek_at(0, 6)? == b"Exif\x00\x00" {
            self.reader.skip(6)?;
            length -= 6;

            let exif_bytes = self.reader.peek_at(0, length)?.to_vec();
            self.exif_data = Some(exif_bytes);
        } else if length > XMP_NAMESPACE_PREFIX.len()
            && self.reader.peek_at(0, XMP_NAMESPACE_PREFIX.len())? == XMP_NAMESPACE_PREFIX
        {
            self.reader.skip(XMP_NAMESPACE_PREFIX.len())?;
            length -= XMP_NAMESPACE_PREFIX.len();
            let xmp_data = self.reader.peek_at(0, length)?.to_vec();
            self.xmp_data = Some(xmp_data);
        } else {
            // 未知 APP1，跳过
        }

        self.reader.skip(length)?;
        Ok(())
    }

    /// 解析 APP2（ICC profile / 其它）。
    fn parse_app2(&mut self) -> Result<(), DecodeError> {
        let mut length = usize::from(self.reader.get_u16_be());

        if length < 2 {
            return Err(DecodeError::Format("Too small app2 segment".to_string()));
        }
        length -= 2;

        if length > 14 && self.reader.peek_at(0, 12)? == *b"ICC_PROFILE\0" {
            length -= 12;
            self.reader.skip(12)?;
            let seq_no = self.reader.read_u8();
            let num_markers = self.reader.read_u8();
            length -= 2;

            let data = self.reader.peek_at(0, length)?.to_vec();

            self.icc_data.push(ICCChunk {
                seq_no,
                num_markers,
                data,
            });
        }

        self.reader.skip(length)?;

        Ok(())
    }

    /// 组装 ICC profile（去掉 APP2 的 14 字节头后按序号拼接）。
    pub(crate) fn icc_profile(&self) -> Option<Vec<u8>> {
        let mut marker_present: [Option<&ICCChunk>; 256] = [None; 256];

        if !self.headers_decoded {
            return None;
        }
        let num_markers = self.icc_data.len();

        if num_markers == 0 || num_markers >= 255 {
            return None;
        }
        for chunk in &self.icc_data {
            if usize::from(chunk.num_markers) != num_markers {
                return None;
            }
            if chunk.seq_no == 0 {
                return None;
            }
            if marker_present[usize::from(chunk.seq_no)].is_some() {
                return None;
            }

            marker_present[usize::from(chunk.seq_no)] = Some(chunk);
        }
        let mut data = Vec::with_capacity(1000);
        for chunk in marker_present.get(1..=num_markers).unwrap() {
            if let Some(ch) = chunk {
                data.extend_from_slice(&ch.data);
            } else {
                return None;
            }
        }

        Some(data)
    }
}

/// 由 SOF 序号得到“不支持方案”描述。
fn unsupported_scheme_desc(int: u8) -> Option<String> {
    Some(
        match int {
            1 => "The library cannot yet decode images encoded using Extended Sequential Huffman encoding scheme yet.",
            3 => "The library cannot yet decode images encoded with Lossless Huffman encoding scheme",
            9 => "The library cannot yet decode Images Encoded with Extended Sequential DCT Arithmetic scheme",
            10 => "The library cannot yet decode images encoded with Progressive DCT Arithmetic scheme",
            11 => "The library cannot yet decode images encoded with Lossless Arithmetic encoding scheme",
            _ => return None,
        }
        .to_string(),
    )
}

/// 计算按 16/8 对齐的填充宽度（C# 语义换主后仅 dormant 引用）。
#[allow(dead_code)]
pub(crate) fn calculate_padded_width(actual_width: usize, sub_sample: SampleRatios) -> usize {
    match sub_sample {
        SampleRatios::None | SampleRatios::V => {
            ((actual_width + 7) / 8) * 8
        }
        SampleRatios::H | SampleRatios::HV => {
            ((actual_width + 15) / 16) * 16
        }
        SampleRatios::Generic(h, _) => {
            ((actual_width + ((h * 8).saturating_sub(1))) / (h * 8)) * (h * 8)
        }
    }
}

/// 反锯齿顺序索引（含防止溢出的 16 个哨兵）。
#[rustfmt::skip]
pub(crate) const UN_ZIGZAG: [usize; 64 + 16] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
    63, 63, 63, 63, 63, 63, 63, 63,
    63, 63, 63, 63, 63, 63, 63, 63
];

/// 量化表反锯齿处理。
fn un_zig_zag<T>(a: &[T]) -> [i32; 64]
where
    T: Default + Copy,
    i32: From<T>,
{
    let mut output = [i32::default(); 64];

    for i in 0..64 {
        output[UN_ZIGZAG[i]] = i32::from(a[i]);
    }

    output
}

/// C# `ZigQ` 融合表长度（64 系数 + 16 哨兵，对齐 `UN_ZIGZAG` 防越界布局）。
pub(crate) const ZIGZAG_QUANT_LEN: usize = 80;

/// 由 natural-order 量化表构建 C# `ZigQ` 融合表
/// （JpegCore.cs:164 `ZigQ[k] = Dezigzag[k]<<16 | Dequant[Dezigzag[k]]`）：
/// 单装载 `zq[pos]` 同时给出 natural-order 目标下标（高 16 位）与量化值（低 16 位），
/// 基线 AC 反量化省一次 `UN_ZIGZAG` + 量化表装载。pos≥64 的哨兵 = natural 63，与
/// `UN_ZIGZAG[min(pos,63)]` 恒等，`min` 兜底随之常消。<br>
/// JPEG DQT 量化为 u8/u16 读取（≤65535），低 16 位恰好装下，`as u32` 保真；乘积/截断
/// 语义与旧 `(v * qt_table[t_pos]) as i16` 在模 2^16 下恒等。
pub(crate) fn build_zigzag_quant(qt_natural: &[i32; DCT_BLOCK]) -> [u32; ZIGZAG_QUANT_LEN] {
    let mut zq = [0u32; ZIGZAG_QUANT_LEN];
    for (i, slot) in zq.iter_mut().enumerate() {
        let natural = UN_ZIGZAG[i]; // i<64 ∈0..63，i≥64 哨兵=63，直接安全
        *slot = ((natural as u32) << 16) | (qt_natural[natural] as u32);
    }
    zq
}
