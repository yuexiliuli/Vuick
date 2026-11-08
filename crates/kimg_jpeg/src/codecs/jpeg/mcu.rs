//! MCU 解码主循环（baseline/progressive；数值核心 = C# 逐字节恒等的 `cs.rs`）。

use core::cmp::{max, min};

use crate::codecs::jpeg::bitstream::{dual_sym_enabled, BitStream, HotStream};
use crate::codecs::jpeg::cs;
use crate::codecs::jpeg::cs::{CsInput, R_ONE_TO_ONE};
use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::huffman::HuffmanTable;
use crate::codecs::jpeg::io::{ByteReader, Marker};
use crate::codecs::jpeg::segment::{
    build_zigzag_quant, ColorSpace, Decoder, SampleRatios, Stage, StageProfile, DCT_BLOCK,
    MAX_COMPONENTS,
};
use crate::codecs::jpeg::upsampler::{upsample_generic, upsample_no_op};

/// MCU 行解码后的续接方式。
enum McuContinuation {
    Ok,
    /// 又发现一个 SOS（非交织多 scan）。
    AnotherSos,
    /// 发现扫描间 setup marker（DHT/DQT/DRI/COM/APP）。
    InterScanMarker(Marker),
    /// 解码中止（损坏图像，非 strict 模式）。
    Terminate,
}

/// 渐进图像 scan 数上限（对齐 zune-core 默认 `max_scans = 100`）。
const MAX_PROGRESSIVE_SCANS: u32 = 100;

/// `KIMG_JPEG_SCAN_PROFILE=1`：渐进解码逐 scan 类别墙钟占比探针（缺省关=零注入）。
/// 只在启用时读取 env（OnceLock 进程内一次）并调用 `Instant::now()`；不 skip 任何 scan、
/// 不改解码语义/位序（纯只读测量）。本机 RDTSC 不可信（K104）→ 只下墙钟。
#[inline]
fn scan_profile_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("KIMG_JPEG_SCAN_PROFILE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    })
}

/// 渐进 scan 类别（探针累加器下标）：DC 类（`spec_start==0`）分 first（`succ_high==0`）/
/// refine（`succ_high>0`）；AC 类（`spec_start>0`）同分。下标 = [dc_first, ac_first,
/// dc_refine, ac_refine]，对应 `scan_ms[4]` 累加顺序。
#[inline]
fn scan_kind(spec_start: u8, succ_high: u8) -> usize {
    let refine = usize::from(succ_high > 0);
    if spec_start == 0 {
        refine * 2
    } else {
        1 + refine * 2
    }
}

/// 逐 scan 探针累加器读数（供 print 用；只在 `scan_profile_enabled()` 时构造）。
#[derive(Default)]
struct ScanProfile {
    ms: [f64; 4],
    n_scans: u32,
}

impl<'a> Decoder<'a> {
    /// 设置分量参数（行宽/量化表/交织信息/采样比例）。
    fn setup_component_params(&mut self) -> Result<(), DecodeError> {
        // Adobe APP14：3 分量 + CMYK → RGB
        if self.components.len() == 3 && self.input_colorspace == ColorSpace::CMYK {
            self.input_colorspace = ColorSpace::RGB;
        }

        for component in &mut self.components {
            // 计算交织图像信息
            self.h_max = max(self.h_max, component.horizontal_sample);
            self.v_max = max(self.v_max, component.vertical_sample);
            self.mcu_width = self.h_max * 8;
            self.mcu_height = self.v_max * 8;
            self.mcu_x = self.width.div_ceil(self.mcu_width);
            self.mcu_y = self.height.div_ceil(self.mcu_height);

            if self.h_max != 1 || self.v_max != 1 {
                self.is_interleaved = true;
            }
            // 把量化表从数组拷贝到分量
            let qt_table = *self.qt_tables[component.quantization_table_number as usize]
                .as_ref()
                .ok_or_else(|| {
                    DecodeError::DqtError(format!(
                        "No quantization table for component id={}",
                        component.id
                    ))
                })?;

            component.quantization_table = qt_table;
            // C# `ZigQ` 融合表：由 natural-order 量化表派生一次，基线 AC 反量化单装载用。
            component.quantization_zq = build_zigzag_quant(&qt_table);
            // 初始 stride 含水平次采样，乘上（每行 MCU 数 × 8）
            component.width_stride *= self.mcu_x * 8;
        }
        {
            // 处理“讨厌的 bug”：Y 分量 2x 采样而其它分量非 1x1
            let mut handle_that_annoying_bug = false;

            if let Some(y_component) = self.components.first() {
                if y_component.horizontal_sample == 2 || y_component.vertical_sample == 2 {
                    handle_that_annoying_bug = true;
                }
            }
            if handle_that_annoying_bug {
                for (idx, comp) in self.components.iter_mut().enumerate() {
                    if idx != 0 && (comp.horizontal_sample != 1 || comp.vertical_sample != 1) {
                        comp.fix_an_annoying_bug = 2;
                    }
                }
            }
        }

        // 检查颜色空间与分量数匹配
        if self.input_colorspace.num_components() > self.components.len() {
            if self.input_colorspace == ColorSpace::YCCK {
                // 3 分量 YCCK 按 YCbCr 处理（与其他解码器一致）
                self.input_colorspace = ColorSpace::YCbCr;
            } else if !self.components.is_empty() {
                // 非 strict：降级为 MultiBand
                self.input_colorspace = ColorSpace::MultiBand(self.components.len());
            } else {
                return Err(DecodeError::Format(format!(
                    "Expected {} number of components but found {}",
                    self.input_colorspace.num_components(),
                    self.components.len()
                )));
            }
        }
        Ok(())
    }

    /// 检查各分量都有 DC/AC Huffman 表。
    fn check_tables(&self) -> Result<(), DecodeError> {
        for component in &self.components {
            self.dc_huffman_tables
                .get(component.dc_huff_table)
                .as_ref()
                .ok_or_else(|| {
                    DecodeError::HuffmanDecode(format!(
                        "No Huffman DC table for component {}",
                        component.id
                    ))
                })?
                .as_ref()
                .ok_or_else(|| {
                    DecodeError::HuffmanDecode(format!(
                        "No DC table for component {}",
                        component.id
                    ))
                })?;

            self.ac_huffman_tables
                .get(component.ac_huff_table)
                .as_ref()
                .ok_or_else(|| {
                    DecodeError::HuffmanDecode(format!(
                        "No Huffman AC table for component {}",
                        component.id
                    ))
                })?
                .as_ref()
                .ok_or_else(|| {
                    DecodeError::HuffmanDecode(format!(
                        "No AC table for component {}",
                        component.id
                    ))
                })?;
        }
        Ok(())
    }

    /// 设置各分量的上采样方案。
    fn set_upsampling(&mut self) -> Result<(), DecodeError> {
        // 无采样则提前返回
        if self.h_max == self.v_max && self.h_max == 1 {
            return Ok(());
        }

        for comp in &mut self.components {
            let hs = self.h_max / comp.horizontal_sample;
            let vs = self.v_max / comp.vertical_sample;

            let (ratio, sampler) = match (hs, vs) {
                (1, 1) => (SampleRatios::None, upsample_no_op as _),
                (2, 1) => (SampleRatios::H, crate::codecs::jpeg::avx2::upsample_horizontal as _),
                (1, 2) => (SampleRatios::V, crate::codecs::jpeg::avx2::upsample_vertical as _),
                (2, 2) => (SampleRatios::HV, crate::codecs::jpeg::avx2::upsample_hv as _),
                (hs, vs) => (SampleRatios::Generic(hs, vs), upsample_generic as _),
            };
            comp.sample_ratio = ratio;
            comp.up_sampler = sampler;
        }

        Ok(())
    }

    /// 灰度下采样图像重置分量参数。
    pub(crate) fn reset_params(&mut self) {
        self.h_max = 1;
        self.v_max = 1;
        self.sample_ratio = SampleRatios::None;
        self.is_interleaved = false;
        self.components[0].vertical_sample = 1;
        self.components[0].width_stride = ((self.width + 7) / 8) * 8;
        self.components[0].horizontal_sample = 1;
    }

    /// baseline 主解码循环。
    pub(crate) fn decode_mcu_ycbcr_baseline(&mut self, pixels: &mut [u8]) -> Result<(), DecodeError> {
        let t_setup = self.stage.timer();

        self.setup_component_params()?;

        self.check_tables()?;

        let (mut mcu_width, mut mcu_height);

        if self.is_interleaved {
            // 设置上采样函数
            self.set_upsampling()?;

            mcu_width = self.mcu_x;
            mcu_height = self.mcu_y;
        } else {
            // 非交织（1x1 次采样）：MCU 数按 /8 计算
            mcu_width = (self.width + 7) / 8;
            mcu_height = (self.height + 7) / 8;
        }

        if self.input_colorspace == ColorSpace::Luma && self.is_interleaved {
            // 灰度下采样图像：重置分量为非交织
            self.reset_params();

            mcu_width = (self.width + 7) / 8;
            mcu_height = (self.height + 7) / 8;
        }
        let width = self.width;

        let mut stream = BitStream::new();
        let mut tmp = [0_i16; DCT_BLOCK];

        let comp_len = self.components.len();

        for (pos, comp) in self.components.iter_mut().enumerate() {
            // 仅标记需要的分量平面（`raw_coeff` 缓冲已随 zune 上采样休眠移除——
            // 解码走 cs 环采样，raw_coeff 分配纯死，2026-11-08）。
            comp.needed = min(
                self.out_colorspace.num_components().saturating_sub(1),
                pos,
            ) == pos
                || comp_len == 4;
        }

        // 初始化 C# 数值核心状态（环/垂直游标/上采样核）。
        self.init_cs_tail();

        // 全部分量都在首个 scan 内 → 可直接逐 MCU 后处理；
        // 否则需先缓冲全部分量到 `progressive_mcus`（u8 采样字节平面）。
        let all_components_in_first_scan = usize::from(self.num_scans) == self.components.len();
        let mut progressive_mcus: [Vec<u8>; 4] = core::array::from_fn(|_| vec![]);

        if !all_components_in_first_scan {
            for (component, mcu) in self.components.iter().zip(&mut progressive_mcus) {
                let len = mcu_width
                    * component.vertical_sample
                    * component.horizontal_sample
                    * mcu_height
                    * 64;
                *mcu = vec![0; len];
            }
        }

        self.stage.stop(Stage::Setup, t_setup);

        'sos: loop {
            for i in 0..mcu_height {
                if stream.overread_by > 0 {
                    pixels.get_mut(self.cs_out_row * width * self.out_bpp()..)
                        .map(|v| v.fill(128));
                    break;
                }

                let t_rows = self.stage.timer();
                let terminate = if all_components_in_first_scan {
                    self.decode_mcu_width::<false>(
                        mcu_width,
                        i,
                        &mut tmp,
                        &mut stream,
                        &mut progressive_mcus,
                    )?
                } else {
                    self.decode_mcu_width::<true>(
                        mcu_width,
                        i,
                        &mut tmp,
                        &mut stream,
                        &mut progressive_mcus,
                    )?
                };
                self.stage.stop(Stage::DecodeRows, t_rows);
                self.stage.n_rows += 1;

                let t_post = self.stage.timer();
                if all_components_in_first_scan {
                    self.post_process(pixels, i, width)?;
                }
                self.stage.stop(Stage::Post, t_post);

                match terminate {
                    McuContinuation::Ok => {}
                    McuContinuation::AnotherSos if all_components_in_first_scan => {
                        return Ok(());
                    }
                    McuContinuation::AnotherSos => continue 'sos,
                    McuContinuation::InterScanMarker(marker) => {
                        // 处理扫描间 marker（DHT/DQT/DRI/COM/APP）
                        if self.advance_to_next_sos(marker, &mut stream)? {
                            continue 'sos;
                        } else {
                            // 命中 EOI
                            break;
                        }
                    }
                    McuContinuation::Terminate => {
                        pixels
                            .get_mut(self.cs_out_row * width * self.out_bpp()..)
                            .map(|v| v.fill(128));
                        return Ok(());
                    }
                }
            }

            break;
        }

        if !all_components_in_first_scan {
            self.finish_baseline_decoding(&progressive_mcus, mcu_width, pixels)?;
        }

        // 确保读到 EOI（UHD 多图分离时允许读不到）
        if !stream.seen_eoi {
            let _ = get_marker(&mut self.reader, &mut stream);
        }

        Ok(())
    }

    /// 非交织多 scan 结束时按分量组装并后处理。
    pub(crate) fn finish_baseline_decoding(
        &mut self,
        block: &[Vec<u8>; MAX_COMPONENTS],
        _mcu_width: usize,
        pixels: &mut [u8],
    ) -> Result<(), DecodeError> {
        let mcu_height = self.mcu_y;
        let width = self.width;

        for (pos, comp) in self.components.iter_mut().enumerate() {
            if min(
                self.out_colorspace.num_components().saturating_sub(1),
                pos,
            ) == pos
                || self.input_colorspace == ColorSpace::YCCK
                || self.input_colorspace == ColorSpace::CMYK
            {
                comp.needed = true;
            } else {
                comp.needed = false;
            }
        }

        self.init_cs_tail();

        for i in 0..mcu_height {
            for (position, component) in self.components.iter_mut().enumerate() {
                if !component.needed {
                    continue;
                }
                // step = 该分量一整行 MCU 的 sample 数
                let step = block[position].len() / mcu_height;
                let slice_opt = block[position].get(i * step..i * step + step);
                let slice = match slice_opt {
                    Some(s) => s,
                    None => return Err(DecodeError::Format("Invalid component data".to_string())),
                };
                // 缓冲平面按绝对行存放；拷入 cs 环对应 MCU 行的一半
                let ring_off = (i & 1) * component.cs_rows_mcu * component.cs_w2;
                for (dst, &src) in component.cs_ring[ring_off..ring_off + step]
                    .iter_mut()
                    .zip(slice.iter())
                {
                    *dst = src as u8;
                }
            }

            self.post_process(pixels, i, width)?;
        }

        Ok(())
    }

    /// 解码一行 MCU。
    fn decode_mcu_width<const PROGRESSIVE: bool>(
        &mut self,
        mcu_width: usize,
        mcu_height: usize,
        tmp: &mut [i16; 64],
        stream: &mut BitStream,
        progressive: &mut [Vec<u8>; 4],
    ) -> Result<McuContinuation, DecodeError> {
        let is_one_by_one = !self.scan_subsampled;

        if is_one_by_one {
            self.inner_decode_mcu_width::<PROGRESSIVE, false>(
                mcu_width,
                mcu_height,
                tmp,
                stream,
                progressive,
            )
        } else {
            self.inner_decode_mcu_width::<PROGRESSIVE, true>(
                mcu_width,
                mcu_height,
                tmp,
                stream,
                progressive,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn inner_decode_mcu_width<const PROGRESSIVE: bool, const SAMPLED: bool>(
        &mut self,
        mcu_width: usize,
        mcu_height: usize,
        tmp: &mut [i16; 64],
        stream: &mut BitStream,
        progressive: &mut [Vec<u8>; 4],
    ) -> Result<McuContinuation, DecodeError> {
        let z_order = self.z_order;
        let z_scans = &z_order[..usize::from(self.num_scans)];

        // 消融开关（仅 profile 模式生效，进程内只读一次）：墙钟差值得 entropy/idct 占比。
        let skip_entropy = StageProfile::skip_entropy();
        let skip_idct = StageProfile::skip_idct();
        // 骨架归因消融：跳过每块 `tmp.fill(0)`（结果错误仅测墙钟，字节恒等必须回实路径）。
        let skip_fill = StageProfile::skip_fill();

        // 寄存器化位流局部状态：整行 MCU 逐块解码共用一份，免逐块经 `stream` 内存往返
        // （省每块 6 字段快照/回写 + 独立函数调用/寄存器盘存）。
        let mut hot = HotStream::from_stream(stream);

        // S3-A：基线跨块 (ab, bl) 寄存器驻留——行首取一次到局部、块环内以 value-return
        // 传递，免每块 entry 载 / EOB·块尾两处写回经 hot 的内存往返（同 P1 decode_prog_*_v
        // 姿势）。hot 自身的 aligned_buffer/bits_left 环内仅作惰性载体：restart / 行尾
        // write_back 前经 `set_ab_bl` 同步，reload 后经 `ab_bl` 取回；buffer/marker/
        // overread_by/seen_eoi 仍由 refill 实时维护（marker 断点逐块时机不变）。
        let (mut ab, mut bl) = hot.ab_bl();

        // 非交织（PROGRESSIVE=true）时按该分量的实际数据单元数迭代
        let mut scan_du_width = if PROGRESSIVE {
            let k = z_scans[0];
            let comp = &self.components[k];
            (self.width * comp.horizontal_sample + self.h_max * 8 - 1) / (self.h_max * 8)
        } else {
            mcu_width
        };
        if PROGRESSIVE && z_scans.len() > 1 {
            let min_du = z_scans
                .iter()
                .map(|&k| self.components[k].width_stride / 8)
                .min()
                .unwrap_or(0);
            scan_du_width = scan_du_width.min(min_du);
        }

        // ① 每分量一次的表解析 + 采样快照（(j,k) 内环外提，P2）：把 `%MAX_COMPONENTS` +
        // `as_ref` + `ok_or_else?` 分支从每 MCU 列热点挪到行首一次。`dc/ac_huffman_tables`
        // 解码期间不再变动（header 已解析、`check_tables` 已先行校验非空），故用原始指针
        // 快照规避与 `handle_rst_main(&mut self)` 的借用冲突（指针不参与借用检查），解引用安全。
        // 有效表缺失 → 行首即返回（与原 `?` 同一 Err；有效图像表必在）。
        let nc = self.components.len().min(MAX_COMPONENTS);
        let mut dc_ptr: [*const HuffmanTable; MAX_COMPONENTS] = [core::ptr::null(); MAX_COMPONENTS];
        let mut ac_ptr: [*const HuffmanTable; MAX_COMPONENTS] = [core::ptr::null(); MAX_COMPONENTS];
        let mut snap_needed = [false; MAX_COMPONENTS];
        let mut snap_vs = [1usize; MAX_COMPONENTS];
        let mut snap_hs = [1usize; MAX_COMPONENTS];
        let mut snap_stride = [0usize; MAX_COMPONENTS];
        for k in 0..nc {
            let c = &self.components[k];
            match self.dc_huffman_tables[c.dc_huff_table % MAX_COMPONENTS].as_ref() {
                Some(t) => dc_ptr[k] = t as *const HuffmanTable,
                None => return Err(DecodeError::Format("DC table not found".to_string())),
            }
            match self.ac_huffman_tables[c.ac_huff_table % MAX_COMPONENTS].as_ref() {
                Some(t) => ac_ptr[k] = t as *const HuffmanTable,
                None => return Err(DecodeError::Format("AC table not found".to_string())),
            }
            snap_needed[k] = c.needed;
            snap_vs[k] = c.vertical_sample;
            snap_hs[k] = c.horizontal_sample;
            snap_stride[k] = c.width_stride;
        }

        // ② restart==0（语料主流）：免 per-MCU `hot.write_back(stream)`——本行末尾一次回写 +
        // marker 处理；per-MCU marker 断点改查 hot（write_back 前后逐字段等价，断点时机不变）。
        // restart!=0 保留原每 MCU 回写 + `handle_rst_main` 路径（RST 语义兜底）。
        let restart = self.restart_interval != 0;

        // P-A2：双符号 AC 打包解码开关——行首一次判定（OnceLock 进程内只读一次），
        // 块环内按寄存器布尔分支分发（预测恒走、无每符号/块原子开销；缺省关 = HEAD
        // 单符号路径逐字节一致）。两条路径各自独立函数，互不影响彼此的代码生成。
        let dual = dual_sym_enabled();

        // S1 稀疏 IDCT 开关——行首一次判定（OnceLock 进程内只读一次，K93 双路径）：
        // `KIMG_JPEG_SPARSE_IDCT=0|false|off` 关 → 回 HEAD 旧全量核（字节恒等）。
        let sparse = sparse_idct_enabled();

        for j in 0..scan_du_width {
            for &k in z_scans {
                let component = &mut self.components[k];

                // SAFETY: dc_ptr/ac_ptr[k] 已在上面由非空 `as_ref` 填充（k<nc），且表在解码
                // 期间不变；仅在本迭代内作只读引用使用。
                let dc_table = unsafe { &*dc_ptr[k] };
                let ac_table = unsafe { &*ac_ptr[k] };

                let qt_table = &component.quantization_table;

                let component_samples_needed = snap_needed[k];

                let v_step = if SAMPLED && !PROGRESSIVE {
                    0..snap_vs[k]
                } else {
                    0..1
                };

                for v_samp in v_step {
                    let h_step = if SAMPLED && !PROGRESSIVE {
                        0..snap_hs[k]
                    } else {
                        0..1
                    };

                    for h_samp in h_step {
                        let t_entropy = self.stage.busy_timer();

                        let result = if skip_entropy {
                            // 消融：跳过熵解码，tmp 填零、len=64（不消费位流；restart 语义靠调用方不触发）
                            if component_samples_needed && !skip_fill {
                                tmp.fill(0);
                            }
                            Ok(64u16)
                        } else if component_samples_needed {
                            // C# `DecodeBlockCore` 每块清零全部 64 系数：不再用「只清首 32」
                            // 的窗口优化——稀疏块会混入上一满块的高位残值，污染全量 f32 IDCT。
                            if !skip_fill {
                                tmp.fill(0);
                            }

                            let r = if dual {
                                hot.decode_mcu_block_dual(
                                    &mut self.reader,
                                    dc_table,
                                    ac_table,
                                    qt_table,
                                    &component.quantization_zq,
                                    tmp,
                                    &mut component.dc_pred,
                                    ab,
                                    bl,
                                )
                            } else {
                                hot.decode_mcu_block(
                                    &mut self.reader,
                                    dc_table,
                                    ac_table,
                                    qt_table,
                                    &component.quantization_zq,
                                    tmp,
                                    &mut component.dc_pred,
                                    ab,
                                    bl,
                                )
                            };
                            match r {
                                Ok((len, a, b)) => {
                                    ab = a;
                                    bl = b;
                                    Ok(len)
                                }
                                Err(e) => Err(e),
                            }
                        } else if dual {
                            match hot.discard_mcu_block_dual(&mut self.reader, dc_table, ac_table, ab, bl) {
                                Ok((len, a, b)) => {
                                    ab = a;
                                    bl = b;
                                    Ok(len)
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            match hot.discard_mcu_block(&mut self.reader, dc_table, ac_table, ab, bl) {
                                Ok((len, a, b)) => {
                                    ab = a;
                                    bl = b;
                                    Ok(len)
                                }
                                Err(e) => Err(e),
                            }
                        };

                        self.stage.busy_stop(Stage::Entropy, t_entropy);
                        if self.stage.counting() {
                            self.stage.n_entropy += 1;
                        }

                        let len = match result {
                            Ok(len) => len,
                            Err(_e) => {
                                // 非 strict：打印并终止，容忍损坏图像
                                return Ok(McuContinuation::Terminate);
                            }
                        };

                        if component_samples_needed {
                            let t_idct = self.stage.busy_timer();
                            if !skip_idct {
                                // 目标字节平面：PROGRESSIVE 缓冲到 `progressive[k]`（整帧样本平面），
                                // 否则写 cs 环当前 MCU 行半区（C# `IdctToRing` 同款偏移）。
                                let (out, off, stride) = if PROGRESSIVE {
                                    let off0 = mcu_height * snap_stride[k] * 8
                                        * snap_vs[k]
                                        + j * 8;
                                    (
                                        &mut progressive[k][..],
                                        off0,
                                        snap_stride[k],
                                    )
                                } else {
                                    let c2 = v_samp * 8;
                                    let c3 = ((j * snap_hs[k]) + h_samp) * 8;
                                    let ring_off = ((mcu_height & 1) * snap_vs[k] * 8 + c2)
                                        * snap_stride[k]
                                        + c3;
                                    (
                                        &mut component.cs_ring[..],
                                        ring_off,
                                        snap_stride[k],
                                    )
                                };
                                if len <= 1 {
                                    cs::idct8x8_dc_only(tmp[0], out, off, stride);
                                } else if sparse {
                                    // S1：熵每块返回 len = zigzag 末位+1 → last=len-1，
                                    // 按 MAX_ROW[MAX_COL][last] 4 态稀疏 IDCT（C# parity）。
                                    cs::idct8x8_sparse(tmp, (len - 1) as usize, out, off, stride);
                                } else {
                                    // 门关（KIMG_JPEG_SPARSE_IDCT=0）：S1 之前的旧全量核，
                                    // 与 HEAD `idct8x8` 路径逐字节一致（同二进制 A/B）。
                                    cs::idct8x8(tmp, out, off, stride);
                                }
                            }
                            self.stage.busy_stop(Stage::Idct, t_idct);
                            if self.stage.counting() {
                                self.stage.n_idct += 1;
                            }
                        }
                    }
                }
            }

            let t_mcu = self.stage.timer();
            self.todo = self.todo.wrapping_sub(1);

            if restart {
                // 原每 MCU 回写 + RST 处理路径（restart≠0 图像，语义逐字节不变）：先把扫描级
                // (ab,bl) 同步进 hot（write_back 依赖 hot.aligned_buffer/bits_left），reload 后取回。
                hot.set_ab_bl(ab, bl);
                hot.write_back(stream);
                if self.todo == 0 {
                    self.handle_rst_main(stream)?;
                    hot.reload(stream);
                    let (a, b) = hot.ab_bl();
                    ab = a;
                    bl = b;
                    continue;
                }
            }

            // 免回写：直接查 hot（= 原 write_back 后查 stream，逐字段等价）；bits_left 读
            // 行级局部 bl（hot 自身 bits_left 环内惰性，环尾才经 set_ab_bl 同步）。
            let bl_zero = bl == 0;
            if hot.marker().is_some() && bl_zero {
                break;
            }
            self.stage.stop(Stage::McuRest, t_mcu);
        }

        // 行尾：先同步扫描级 (ab,bl) 进 hot 再统一 write_back。
        hot.set_ab_bl(ab, bl);
        hot.write_back(stream);
        self.check_stream_marker_after_mcu_width(stream)
    }

    /// 一行 MCU 结束后处理流内 marker。
    fn check_stream_marker_after_mcu_width(
        &mut self,
        stream: &mut BitStream,
    ) -> Result<McuContinuation, DecodeError> {
        // Jpeg 规范只允许熵流中出现 RST，但 libjpeg-turbo 也容忍其它 marker
        if let Some(m) = stream.marker {
            if m == Marker::EOI {
                // 确认并忽略 EOI
                stream.marker.take();
                stream.seen_eoi = true;
            } else if let Marker::RST(_) = m {
                if self.todo == 0 {
                    self.handle_rst(stream)?;
                }
            } else if let Marker::SOS = m {
                self.parse_marker_inner(m)?;
                stream.marker.take();
                stream.reset();
                return Ok(McuContinuation::AnotherSos);
            } else if matches!(m, Marker::DHT | Marker::DQT | Marker::DRI | Marker::COM)
                || matches!(m, Marker::APP(_))
            {
                // 非交织图像 scan 间可出现 setup marker
                stream.marker.take();
                return Ok(McuContinuation::InterScanMarker(m));
            } else {
                // 非 strict：解析后终止
                self.parse_marker_inner(m)?;
                stream.marker.take();
                stream.reset();
                return Ok(McuContinuation::Terminate);
            }
        }

        Ok(McuContinuation::Ok)
    }

    /// 扫描到下一个 SOS（沿途解析 setup marker）。
    fn advance_to_next_sos(
        &mut self,
        first_marker: Marker,
        stream: &mut BitStream,
    ) -> Result<bool, DecodeError> {
        const MAX_INTER_SCAN_MARKERS: usize = 64;

        self.parse_marker_inner(first_marker)?;
        stream.reset();

        for _ in 0..MAX_INTER_SCAN_MARKERS {
            let marker = get_marker(&mut self.reader, stream)?;

            match marker {
                Marker::SOS => {
                    self.parse_marker_inner(Marker::SOS)?;
                    stream.reset();
                    return Ok(true);
                }
                Marker::EOI => {
                    stream.seen_eoi = true;
                    return Ok(false);
                }
                Marker::DHT | Marker::DQT | Marker::DRI | Marker::COM => {
                    self.parse_marker_inner(marker)?;
                }
                Marker::APP(_) => {
                    self.parse_marker_inner(marker)?;
                }
                other => {
                    // 非 strict：跳过未知 marker
                    let length = self.reader.get_u16_be_err()?;
                    if length >= 2 {
                        self.reader.skip((length - 2) as usize)?;
                    }
                    let _ = other;
                }
            }
        }

        Err(DecodeError::Format(
            "Too many markers between scans (exceeded limit of 64)".to_string(),
        ))
    }

    /// 处理 RST marker（未用重启则无操作）。
    #[cold]
    pub(crate) fn handle_rst(&mut self, stream: &mut BitStream) -> Result<(), DecodeError> {
        self.todo = self.restart_interval;

        if let Some(marker) = stream.marker {
            match marker {
                Marker::RST(_) => {
                    stream.reset();
                    self.components.iter_mut().for_each(|x| x.dc_pred = 0);
                }
                Marker::EOI => {
                    // 静默通过
                }
                // 可合法出现在重启边界的 marker 留给调用方
                Marker::SOS | Marker::DHT | Marker::DQT | Marker::DRI | Marker::COM
                | Marker::APP(_) => {}
                _ => {}
            }
        }
        Ok(())
    }

    #[cold]
    pub(crate) fn handle_rst_main(&mut self, stream: &mut BitStream) -> Result<(), DecodeError> {
        if self.todo == 0 {
            stream.refill(&mut self.reader)?;
        }

        if self.todo == 0
            && self.restart_interval != 0
            && stream.marker.is_none()
            && !stream.seen_eoi
        {
            // 若无 marker 且应重置 RST，则寻找 marker
            let marker = get_marker(&mut self.reader, stream);

            if let Ok(marker) = marker {
                stream.marker = Some(marker);
            } else {
                // 最后一段可能没有 RST 标记（由 EOI 收尾），警告即可
            }
        }
        if self.todo == 0 {
            self.handle_rst(stream)?;
        }
        Ok(())
    }

    /// 输出每像素字节数（对齐 C# `_outBpp`）。
    fn out_bpp(&self) -> usize {
        self.out_colorspace.num_components()
    }

    /// 初始化 C# 数值核心状态：环 / 垂直游标 / 上采样核（每 decode 一次，幂等）。
    fn init_cs_tail(&mut self) {
        if self.cs_inited {
            return;
        }
        self.cs_inited = true;
        self.cs_out_row = 0;

        let in_cs = self.input_colorspace;
        let is_rgb = in_cs == ColorSpace::RGB;
        self.cs_is_rgb_input = is_rgb;
        self.cs_input_kind = match in_cs {
            ColorSpace::Luma | ColorSpace::LumaA => CsInput::Luma,
            ColorSpace::RGB => CsInput::Rgb,
            ColorSpace::YCbCr => CsInput::Ycc,
            ColorSpace::CMYK => CsInput::Cmyk,
            ColorSpace::YCCK => CsInput::Ycck,
            _ => CsInput::Other,
        };

        let comps = self.components.len();
        let h_max = self.h_max;
        let v_max = self.v_max;
        let width = self.width;
        let height = self.height;

        // 各分量 Hs/Vs 先算好，避免逐分量的可变借用贴着 h_max 读取。
        let ratios: Vec<(usize, usize)> = self
            .components
            .iter()
            .map(|c| (h_max / c.horizontal_sample, v_max / c.vertical_sample))
            .collect();

        for (comp, (hs, vs)) in self.components.iter_mut().zip(ratios) {
            comp.cs_hs = hs;
            comp.cs_vs = vs;
            comp.cs_resample_kind = match (hs, vs) {
                (1, 1) => R_ONE_TO_ONE,
                (1, 2) => cs::R_V2,
                (2, 1) => cs::R_H2,
                (2, 2) => cs::R_HV2,
                _ => cs::R_GENERIC,
            };
            comp.cs_w2 = comp.width_stride;
            comp.cs_wlores = width.div_ceil(hs);
            comp.cs_ys_scaled = (height * comp.vertical_sample + v_max - 1) / v_max;
            comp.cs_rows_mcu = comp.vertical_sample * 8;
            comp.cs_ring_rows = 2 * comp.cs_rows_mcu;
            let n_out = cs::output_count(comp.cs_resample_kind, comp.cs_wlores, hs) + 3;
            comp.cs_line_buf8 = vec![0u8; n_out];
            comp.cs_line_buf16 = vec![0u16; n_out];

            if comp.needed {
                // C# `Ring.Clear()`：未覆盖的 padding 样本确定性为 0。
                comp.cs_ring = vec![0u8; comp.cs_ring_rows * comp.cs_w2];
            } else {
                comp.cs_ring.clear();
            }
            comp.cs_line0 = 0;
            comp.cs_line1 = 0;
            // C# `PrepareOutput`：`YStep = Vs >> 1`（源码为准，勿改成 0——顶部两行的
            // ¾/¼ 混合相位依赖它）。
            comp.cs_ystep = vs >> 1;
            comp.cs_ypos = 0;
            comp.cs_rows_avail = 0;
            comp.cs_chroma_ready = false;
            comp.cs_row_is_ring = true;
            comp.cs_row_near = 0;
        }

        // `_chroma16`：ycc 且分量 1/2 任一生效核非 OneToOne。
        let ycc = (comps == 3 && !is_rgb) || in_cs == ColorSpace::YCCK;
        self.cs_chroma16 = ycc
            && (self.components.get(1).is_some_and(|c| c.cs_resample_kind != R_ONE_TO_ONE)
                || self.components.get(2).is_some_and(|c| c.cs_resample_kind != R_ONE_TO_ONE));
    }

    /// 逐行后处理：等价 C# `CommitMcuRow` + `EmitRows` + `EmitRow`。
    /// `m` = 当前 MCU 行；每调一次先提交该行的 `RowsAvail`，再尽可能多地输出行。
    fn post_process(&mut self, pixels: &mut [u8], m: usize, width: usize) -> Result<(), DecodeError> {
        for comp in &mut self.components {
            comp.cs_rows_avail = (m + 1) * comp.cs_rows_mcu;
        }

        let out_bpp = self.out_colorspace.num_components();
        let chroma16 = self.cs_chroma16;
        let input_kind = self.cs_input_kind;
        let out_x = width;
        let height = self.height;
        let out_bytes = out_x * out_bpp;

        while self.cs_out_row < height {
            // C# `EmitRows` 门：任一分量 Line1 未就绪 → 等下一 MCU 行
            let ready = self.components.iter().all(|c| c.cs_line1 < c.cs_rows_avail);
            if !ready {
                break;
            }

            // 逐分量：选近/远源行 → 上采样（16× 或 8 位）→ AdvanceLine
            for k in 0..self.components.len() {
                let comp = &mut self.components[k];
                if !comp.needed {
                    comp.cs_advance_line();
                    comp.cs_row_is_ring = true;
                    comp.cs_row_near = 0;
                    continue;
                }
                let y_bot = comp.cs_ystep >= (comp.cs_vs >> 1);
                let in_near = comp.cs_ring_offset(if y_bot { comp.cs_line1 } else { comp.cs_line0 });
                let in_far = comp.cs_ring_offset(if y_bot { comp.cs_line0 } else { comp.cs_line1 });

                if chroma16 && (k == 1 || k == 2) {
                    cs::row16(
                        comp.cs_resample_kind,
                        &mut comp.cs_line_buf16,
                        &comp.cs_ring,
                        in_near,
                        in_far,
                        comp.cs_wlores,
                        comp.cs_hs,
                    );
                    comp.cs_chroma_ready = true;
                    comp.cs_row_is_ring = false;
                } else if comp.cs_resample_kind == R_ONE_TO_ONE {
                    comp.cs_row_is_ring = true;
                    comp.cs_row_near = in_near;
                } else {
                    cs::row8(
                        comp.cs_resample_kind,
                        &mut comp.cs_line_buf8,
                        &mut comp.cs_line_buf16,
                        &comp.cs_ring,
                        in_near,
                        in_far,
                        comp.cs_wlores,
                        comp.cs_hs,
                    );
                    comp.cs_row_is_ring = false;
                }
                comp.cs_advance_line();
            }

            let j = self.cs_out_row;
            let row_out = match pixels.get_mut(j * out_bytes..j * out_bytes + out_bytes) {
                Some(s) => s,
                None => return Err(DecodeError::Format("Output row out of bounds".to_string())),
            };

            // 组每分量 8 位行源（环直读或 line_buf8）
            let mut rows: [&[u8]; 4] = [&[], &[], &[], &[]];
            for k in 0..self.components.len().min(4) {
                let comp = &self.components[k];
                if comp.cs_row_is_ring {
                    rows[k] = comp.cs_ring.get(comp.cs_row_near..).unwrap_or(&[]);
                } else {
                    rows[k] = &comp.cs_line_buf8;
                }
            }
            // 16× 色度行源（chroma16 时）
            let mut rows16: [&[u16]; 4] = [&[], &[], &[], &[]];
            if chroma16 {
                for k in 1..self.components.len().min(3) {
                    rows16[k] = &self.components[k].cs_line_buf16;
                }
            }

            cs::pack_output_row(input_kind, out_bpp, chroma16, &rows, &rows16, row_out, out_x);
            self.cs_out_row += 1;
        }

        Ok(())
    }

    /// 重置渐进参数（每个 scan 开始前调用，与 zune `reset_prog_params` 对齐）。
    fn reset_prog_params(&mut self, stream: &mut BitStream) {
        stream.reset();
        self.components.iter_mut().for_each(|x| x.dc_pred = 0);
        self.todo = if self.restart_interval != 0 {
            self.restart_interval
        } else {
            usize::MAX
        };
    }

    /// 渐进模式主解码循环（1:1 复刻 zune `decode_mcu_ycbcr_progressive`）。
    #[allow(clippy::too_many_lines)]
    pub(crate) fn decode_mcu_ycbcr_progressive(&mut self, pixels: &mut [u8]) -> Result<(), DecodeError> {
        self.setup_component_params()?;

        // 各分量的整帧系数缓冲（未反量化、未 IDCT 的原始 DCT 系数）
        let mut block: [Vec<i16>; MAX_COMPONENTS] = core::array::from_fn(|_| vec![]);
        // P2：逐块非零掩码——仅含 AC 显著位，位 p = zigzag 系数序 p（块内非零系数），AC-first
        // 写系数时置位、AC-refine 稀疏游走复用并补写新显著位；DC 扫走独立路径不涉 bit 0
        // （decode_prog_dc_first_v/refine_v 不接收 mask）。内存 = 每块 8B，并行于系数缓冲。
        let mut prog_mask: [Vec<u64>; MAX_COMPONENTS] = core::array::from_fn(|_| vec![]);
        let mut mcu_width;
        let mut mcu_height;
        let mut seen_scans = 1;

        if self.input_colorspace == ColorSpace::Luma && self.is_interleaved {
            // 灰度下调采样图像：重置为无采样形态
            self.reset_params();
        }

        if self.is_interleaved {
            self.set_upsampling()?;
        }

        if self.is_interleaved {
            mcu_width = self.mcu_x;
            mcu_height = self.mcu_y;
        } else {
            mcu_width = (self.width + 7) / 8;
            mcu_height = (self.height + 7) / 8;
        }

        // 特定图像（交织 YCbCr → 灰）需按采样比例修正 mcu_height
        if self.is_interleaved
            && self.input_colorspace.num_components() > 1
            && self.out_colorspace.num_components() == 1
            && (self.sample_ratio == SampleRatios::V || self.sample_ratio == SampleRatios::HV)
        {
            mcu_height *= self.v_max;
            mcu_height /= self.h_max;
            self.coeff = 2;
        }

        mcu_width *= 64;

        for i in 0..self.input_colorspace.num_components() {
            let comp = &self.components[i];
            let len = mcu_width * comp.vertical_sample * comp.horizontal_sample * mcu_height;

            block[i] = vec![0; len];
            prog_mask[i] = vec![0; len / 64];
        }

        let mut stream = BitStream::new_progressive(self.succ_low, self.spec_start, self.spec_end);

        // 逐 scan 墙钟占比探针（env 门控 `KIMG_JPEG_SCAN_PROFILE=1`；缺省关=零注入，不改语义）
        let scan_prof = scan_profile_enabled();
        let mut prof = ScanProfile::default();
        let t_scan0 = if scan_prof {
            Some(std::time::Instant::now())
        } else {
            None
        };

        // 首个 scan 已被 headers 阶段解析，直接解熵数据
        let t_e0 = if scan_prof {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let result = self.parse_entropy_coded_data(&mut stream, &mut block, &mut prog_mask);
        if scan_prof {
            if let Some(t) = t_e0 {
                let k = scan_kind(self.spec_start, self.succ_high);
                prof.ms[k] += t.elapsed().as_secs_f64() * 1000.0;
                prof.n_scans += 1;
            }
        }
        let first_err = result.is_err();

        if !first_err {
            // 取出终结首个 scan 的 marker
            let mut marker = stream.marker.take().ok_or_else(|| {
                DecodeError::Format("Marker missing where expected".to_string())
            })?;

            // 循环处理后续 scan；直到 EOI
            'eoi: while marker != Marker::EOI {
                match marker {
                    Marker::SOS => {
                        self.parse_marker_inner(Marker::SOS)?;

                        stream.update_progressive_params(
                            self.succ_high,
                            self.succ_low,
                            self.spec_start,
                            self.spec_end,
                        );

                        let t_es = if scan_prof {
                            Some(std::time::Instant::now())
                        } else {
                            None
                        };
                        let result =
                            self.parse_entropy_coded_data(&mut stream, &mut block, &mut prog_mask);
                        if scan_prof {
                            if let Some(t) = t_es {
                                let k = scan_kind(self.spec_start, self.succ_high);
                                prof.ms[k] += t.elapsed().as_secs_f64() * 1000.0;
                                prof.n_scans += 1;
                            }
                        }

                        if result.is_err() {
                            // 非 strict：跳出并收尾
                            break 'eoi;
                        }

                        match get_marker(&mut self.reader, &mut stream) {
                            Ok(marker_n) => {
                                marker = marker_n;
                                seen_scans += 1;
                                if seen_scans > MAX_PROGRESSIVE_SCANS {
                                    return Err(DecodeError::Format(format!(
                                        "Too many scans, exceeded limit of {}",
                                        MAX_PROGRESSIVE_SCANS
                                    )));
                                }

                                stream.reset();
                                continue 'eoi;
                            }
                            Err(_msg) => {
                                // 非 strict：跳出
                                break 'eoi;
                            }
                        }
                    }
                    Marker::RST(_n) => {
                        self.handle_rst(&mut stream)?;
                    }
                    _ => {
                        // DHT/DQT/DRI/COM/APP 等 scan 间 setup marker
                        self.parse_marker_inner(marker)?;
                    }
                }

                match get_marker(&mut self.reader, &mut stream) {
                    Ok(marker_n) => {
                        marker = marker_n;
                    }
                    Err(_e) => {
                        // 非 strict：跳出，容忍部分损坏图像
                        break 'eoi;
                    }
                }
            }
        }

        // 探针：打印 scan 类别墙钟占比（env 门控；关联文件名由测量侧在解码前打印的
        // `SCANP FILE <name>` 标记行给出，与 STAGE FILE/STAGE-DECODE 同款关联口径）
        if scan_prof {
            let total_ms = t_scan0.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
            let sum: f64 = prof.ms.iter().sum();
            let other = (total_ms - sum).max(0.0);
            let pct = |v: f64| {
                if total_ms > 1e-6 {
                    v / total_ms * 100.0
                } else {
                    0.0
                }
            };
            println!(
                "SCANP total={total_ms:.2}ms dc_first={:.2}({:.1}%) ac_first={:.2}({:.1}%) ac_refine={:.2}({:.1}%) dc_refine={:.2}({:.1}%) n_scans={} other_scan={other:.2}({:.1}%)",
                prof.ms[0],
                pct(prof.ms[0]),
                prof.ms[1],
                pct(prof.ms[1]),
                prof.ms[3],
                pct(prof.ms[3]),
                prof.ms[2],
                pct(prof.ms[2]),
                prof.n_scans,
                pct(other),
            );
        }

        self.finish_progressive_decoding(&block, &prog_mask, pixels)
    }

    /// 解单个 scan 的熵编码数据（1:1 复刻 zune `parse_entropy_coded_data`；P1 A′ 改造；
    /// P2 带逐块非零掩码 `prog_mask` 供 AC-first 置位 / AC-refine 稀疏游走）。
    #[allow(clippy::too_many_lines, clippy::cast_sign_loss)]
    fn parse_entropy_coded_data(
        &mut self,
        stream: &mut BitStream,
        buffer: &mut [Vec<i16>; MAX_COMPONENTS],
        prog_mask: &mut [Vec<u64>; MAX_COMPONENTS],
    ) -> Result<(), DecodeError> {
        self.reset_prog_params(stream);

        if usize::from(self.num_scans) > self.input_colorspace.num_components() {
            return Err(DecodeError::Format(format!(
                "Number of scans {} cannot be greater than number of components, {}",
                self.num_scans,
                self.input_colorspace.num_components()
            )));
        }

        // P1 前置探针修复：skip_entropy 消融在渐进上不再 panic。skip_e 跳过实际熵解码，
        // 但仍以字节扫描推进位流到下一真实 marker（restart 图的中途 RST marker 由外层
        // scan 循环逐段消费），使 scan 层 marker 语义正常收尾——此前 skip_e 完全不消费位流，
        // `stream.marker` 恒 None，`decode_mcu_ycbcr_progressive` 在 `marker.take()`
        // （mcu.rs:1015）处 panic「Marker missing where expected」。AC-refine 的位消费依赖
        // 块内既有非零系数（refine 位只对非零系数读），无法「只跳过系数写入」仍保持位序，
        // 故用 get_marker 盲扫推进（仅测量模式，entropy 墙钟趋零出真值）。
        if StageProfile::skip_entropy() {
            match get_marker(&mut self.reader, stream) {
                Ok(m) => stream.marker = Some(m),
                Err(_) => {
                    // 截断/损坏：置 EOI 让外层 scan 循环正常收尾（仅测量模式）
                    stream.seen_eoi = true;
                    stream.marker = Some(Marker::EOI);
                }
            }
            return Ok(());
        }

        if self.num_scans == 1 {
            // 安全检查：单个非交织 scan 不能同时含 DC 与 AC 谱段
            if self.spec_end != 0 && self.spec_start == 0 {
                return Err(DecodeError::Format(
                    "Can't merge DC and AC corrupt jpeg".to_string(),
                ));
            }

            // 非交织：逐块按常规扫描顺序解码
            let comp_k = self.z_order[0];

            if comp_k >= self.components.len() {
                return Err(DecodeError::Format(format!(
                    "Cannot find component {comp_k}, corrupt image"
                )));
            }

            let comp = &self.components[comp_k];
            let width_stride = comp.width_stride;

            // 非交织 scan 迭代该分量实际的数据单元网格
            let mcu_width = (self.width * comp.horizontal_sample).div_ceil(self.h_max * 8);
            let mcu_height = (self.height * comp.vertical_sample).div_ceil(self.v_max * 8);

            // P1：每 scan 一次的表解析 + 指针快照（把 `%MAX_COMPONENTS` + `as_ref` +
            // `ok_or_else?` 分支从每块热点挪到 scan 首一次）。`dc/ac_huffman_tables` 解码期间
            // 不再变动 → 原始指针快照解引用安全（与 baseline 行首 dc_ptr/ac_ptr 同款）；
            // 有效表缺失 → scan 首即返回（与原每块 `?` 同一 Err 文本）。
            let dc_idx = comp.dc_huff_table & (MAX_COMPONENTS - 1);
            let dc_ptr: *const HuffmanTable = match self.dc_huffman_tables[dc_idx].as_ref() {
                Some(t) => t as *const HuffmanTable,
                None => {
                    return Err(DecodeError::Format("DC table not found".to_string()));
                }
            };
            let ac_idx = comp.ac_huff_table;
            let ac_ptr: *const HuffmanTable = match self
                .ac_huffman_tables
                .get(ac_idx)
                .ok_or_else(|| {
                    DecodeError::Format(format!("No huffman table for component:{ac_idx}"))
                })?
                .as_ref()
            {
                Some(t) => t as *const HuffmanTable,
                None => {
                    return Err(DecodeError::Format(format!(
                        "Huffman table at index  {ac_idx} not initialized"
                    )))
                }
            };

            // P1：ab/bl 扫描级局部寄存器驻留（value-return refill/decode 同款），块环以
            // 值返回方式在块间传递，不逐块回写 `stream`。restart 图需逐块回写 +
            // handle_rst_main + reload（RST 语义兜底，与 baseline inner_decode_mcu_width
            // restart!=0 姿势同款）；restart==0（语料主流）免回写，marker 由 refill/decode
            // 设进 stream，块间不回写。
            let restart = self.restart_interval != 0;
            let (mut ab, mut bl) = stream.ab_bl();

            for i in 0..mcu_height {
                for j in 0..mcu_width {
                    let t_entropy = self.stage.busy_timer();
                    if self.spec_start != 0 && self.succ_high == 0 && stream.eob_run > 0 {
                        // 在此处理 EOB RUN 的整块跳过
                        stream.eob_run -= 1;
                    } else {
                        let start = 64 * (j + i * (width_stride / 8));
                        // P2：本块非零掩码槽（位 p = zigzag 系数序 p，仅 AC 显著位）；与 data
                        // 同块索引（start 恒为 64 倍数）。
                        let blk_mask = &mut prog_mask[comp_k][start >> 6];

                        let data: &mut [i16; 64] = buffer
                            .get_mut(comp_k)
                            .ok_or_else(|| DecodeError::Format("Invalid image".to_string()))?
                            .get_mut(start..start + 64)
                            .ok_or_else(|| DecodeError::Format("Slice to Small".to_string()))?
                            .try_into()
                            .map_err(|_| DecodeError::Format("Slice to Small".to_string()))?;

                        if self.spec_start == 0 {
                            // DC 谱段（含 DC refine）
                            let dc_pred = &mut self.components[comp_k].dc_pred;

                            if self.succ_high == 0 {
                                // SAFETY: dc_ptr/已由上非空 `as_ref` 快照（comp_k < nc 且在此
                                // scan 固定），表解码期间不变；仅本迭代内作只读引用使用。
                                let (a, b) = stream.decode_prog_dc_first_v(
                                    &mut self.reader,
                                    unsafe { &*dc_ptr },
                                    &mut data[0],
                                    dc_pred,
                                    ab,
                                    bl,
                                )?;
                                ab = a;
                                bl = b;
                            } else {
                                let (a, b) = stream.decode_prog_dc_refine_v(
                                    &mut self.reader,
                                    &mut data[0],
                                    ab,
                                    bl,
                                )?;
                                ab = a;
                                bl = b;
                            }
                        } else {
                            // AC 谱段（含 AC refine）
                            let (a, b) = if self.succ_high == 0 {
                                // SAFETY: ac_ptr 同上（ac_huff_table 冲突仅照原 Err 文本）。
                                stream.decode_mcu_ac_first_v(
                                    &mut self.reader,
                                    unsafe { &*ac_ptr },
                                    data,
                                    blk_mask,
                                    ab,
                                    bl,
                                )?
                            } else {
                                stream.decode_mcu_ac_refine_v(
                                    &mut self.reader,
                                    unsafe { &*ac_ptr },
                                    data,
                                    blk_mask,
                                    ab,
                                    bl,
                                )?
                            };
                            ab = a;
                            bl = b;
                        }
                    }
                    self.stage.busy_stop(Stage::Entropy, t_entropy);

                    self.todo = self.todo.wrapping_sub(1);

                    if restart {
                        // RST 语义兜底：逐块回写 + handle_rst_main + reload（stream 被改则同步回 ab/bl）
                        stream.set_ab_bl(ab, bl);
                        self.handle_rst_main(stream)?;
                        (ab, bl) = stream.ab_bl();
                    } else {
                        // restart==0：handle_rst_main 恒 no-op（todo 首值为 usize::MAX，不会
                        // 递减到 0），免每块调用；marker 在 decode 的 refill 内设进 stream。
                    }
                }
            }
            stream.set_ab_bl(ab, bl);
        } else {
            if self.spec_end != 0 {
                return Err(DecodeError::HuffmanDecode(
                    "Can't merge dc and AC corrupt jpeg".to_string(),
                ));
            }

            // 预校验交织 scan 中各分量 DC 表存在
            for k in 0..self.num_scans {
                let n = self.z_order[usize::from(k)];

                if n >= self.components.len() {
                    return Err(DecodeError::Format(format!(
                        "Cannot find component {n}, corrupt image"
                    )));
                }

                let component = &mut self.components[n];
                let _ = self
                    .dc_huffman_tables
                    .get(component.dc_huff_table)
                    .ok_or_else(|| {
                        DecodeError::Format(format!(
                            "No huffman table for component:{}",
                            component.dc_huff_table
                        ))
                    })?
                    .as_ref()
                    .ok_or_else(|| {
                        DecodeError::Format(format!(
                            "Huffman table at index  {} not initialized",
                            component.dc_huff_table
                        ))
                    })?;
            }

            // P1：交织 DC scan 的表指针快照（每 scan 一次；dc_ptr 按分量下标，表在位期间不变）。
            let mut dc_ptr: [*const HuffmanTable; MAX_COMPONENTS] = [core::ptr::null(); MAX_COMPONENTS];
            for k in 0..self.num_scans {
                let n = self.z_order[usize::from(k)];
                let dci = self.components[n].dc_huff_table;
                match self
                    .dc_huffman_tables
                    .get(dci)
                    .ok_or_else(|| {
                        DecodeError::Format(format!("No huffman table for component:{dci}"))
                    })?
                    .as_ref()
                {
                    Some(t) => dc_ptr[n] = t as *const HuffmanTable,
                    None => {
                        return Err(DecodeError::Format(format!(
                            "Huffman table at index  {dci} not initialized"
                        )))
                    }
                }
            }

            // C1：交织 DC scan：逐 MCU 逐分量解码（仅 DC 允许交织）。ab/bl 提升为扫描级局部
            // 寄存器（`_v` value-return A′ 变体，与单分量路径 mcu.rs:1305/1316 同构），消除
            // 每块经 `&mut BitStream` 的 6 字段快照/回写与 store-forward 链。
            // restart==0：免每 MCU `#[cold] handle_rst_main`（todo=usize::MAX 恒不触 RST，
            // 与单分量路径 mcu.rs:1355 同款门控）；restart≠0：保留每 MCU 逐块回写 +
            // handle_rst_main + reload（RST 语义兜底）。
            let restart = self.restart_interval != 0;
            let (mut ab, mut bl) = stream.ab_bl();
            for i in 0..self.mcu_y {
                for j in 0..self.mcu_x {
                    for k in 0..self.num_scans {
                        let n = self.z_order[usize::from(k)];

                        let component = &mut self.components[n];

                        // SAFETY: dc_ptr[n] 上方已按 `dc_huffman_tables.get` 语义非空填充
                        // （n 在本 scan 分量集内），表解码期间不变。
                        let huff_table = unsafe { &*dc_ptr[n] };

                        for v_samp in 0..component.vertical_sample {
                            for h_samp in 0..component.horizontal_sample {
                                let x2 = j * component.horizontal_sample + h_samp;
                                let y2 = i * component.vertical_sample + v_samp;
                                let position = 64 * (x2 + y2 * component.width_stride / 8);

                                let buf_n = &mut buffer[n];

                                let data = buf_n.get_mut(position).ok_or_else(|| {
                                    DecodeError::Format("Invalid image".to_string())
                                })?;

                                let t_entropy = self.stage.busy_timer();
                                let (a, b) = if self.succ_high == 0 {
                                    stream.decode_prog_dc_first_v(
                                        &mut self.reader,
                                        huff_table,
                                        data,
                                        &mut component.dc_pred,
                                        ab,
                                        bl,
                                    )?
                                } else {
                                    stream.decode_prog_dc_refine_v(&mut self.reader, data, ab, bl)?
                                };
                                ab = a;
                                bl = b;
                                self.stage.busy_stop(Stage::Entropy, t_entropy);
                                if self.stage.counting() {
                                    self.stage.n_entropy += 1;
                                }
                            }
                        }
                    }

                    self.todo = self.todo.wrapping_sub(1);

                    if restart {
                        // RST 语义兜底：逐 MCU 回写 + handle_rst_main + reload（stream 被改
                        // 则同步回 ab/bl；与单分量路径 mcu.rs:1355 同款）。
                        stream.set_ab_bl(ab, bl);
                        self.handle_rst_main(stream)?;
                        (ab, bl) = stream.ab_bl();
                    }
                }
            }
            stream.set_ab_bl(ab, bl);
        }

        Ok(())
    }

    /// 渐进收尾：整帧反量化 → f32 IDCT → 后处理（C# `EmitCoefficients` 同款）。
    ///
    /// `prog_mask`（P2 逐块非零掩码）与本函数平铺块序逐位对齐：对分量 `position`、MCU 行
    /// `i`、竖直子块 `k`、水平子块 `j`，`flat = (i*vertical_sample + k)*mcu_x + j` 即
    /// `prog_mask[position]` 下标（熵段非交织 AC scan 以同序 `start>>6` 写掩码，DC scan 不
    /// 涉 bit0；`block`/`prog_mask` 由同一 `len` 分配，灰度下采样修正对两者一致）。
    #[allow(clippy::too_many_lines)]
    fn finish_progressive_decoding(
        &mut self,
        block: &[Vec<i16>; MAX_COMPONENTS],
        prog_mask: &[Vec<u64>; MAX_COMPONENTS],
        pixels: &mut [u8],
    ) -> Result<(), DecodeError> {
        let mcu_height = if self.is_interleaved {
            self.mcu_y
        } else {
            // 非交织（1x1 采样）：MCU 数以宽高除以 8 计
            (self.height + 7) / 8
        };

        let width = self.width;
        let mut tmp = [0_i16; DCT_BLOCK];

        for (pos, comp) in self.components.iter_mut().enumerate() {
            // 仅标记需要的分量平面（`raw_coeff` 缓冲已随 zune 上采样休眠移除，2026-11-08）。
            comp.needed = min(
                self.out_colorspace.num_components().saturating_sub(1),
                pos,
            ) == pos
                || self.input_colorspace == ColorSpace::YCCK
                || self.input_colorspace == ColorSpace::CMYK;
        }

        self.init_cs_tail();

        // 逐分量 natural-order i16 量化系数（`q as i16` = 低 16 位模表示，与 i32 乘积
        // 后截断写回 i16 在模 2^16 下恒等——16 位精度量化表同样成立）。
        let mut qt16: [[i16; 64]; MAX_COMPONENTS] = [[0; 64]; MAX_COMPONENTS];
        for (position, component) in self.components.iter().enumerate() {
            for (dst, &q) in qt16[position]
                .iter_mut()
                .zip(component.quantization_table.iter())
            {
                *dst = q as i16;
            }
        }

        // 反量化、IDCT、输出（逐 MCU 行）
        // A/B 门控（进程内一次，K93 姿势）：
        //   `KIMG_JPEG_PROG_FASTPATH=0` → 仅向量化反量化、恒全量 IDCT（最早旧路径）；
        //   `KIMG_JPEG_FINISH_FUSED=0` → 回退 HEAD 6de52fe 的旧 finish（dequant_block 全量
        //     算 + ac_zero 判定路由），缺省 → 候选 A+B（prog_mask 预判 DC-only 免全量 dequant
        //     + fused「反量化∘IDCT」内核）。两开关都开 = 本次默认。
        // popcount 探针（`KIMG_JPEG_PROG_MASK_PROFILE=1`，缺省关=零注入）只读计数不改路径。
        let fused = finish_fused_enabled();
        let fastpath = prog_fastpath_enabled();
        let maskp = popmask_profile_enabled();
        // S4 稀疏 IDCT 开关（K93 双路径，同二进制 A/B）：同 baseline 的
        // `KIMG_JPEG_SPARSE_IDCT`，门关 → fused 稀疏核回退 fused 全量核（逐字节一致）。
        let sparse = sparse_idct_enabled();
        let mut pop = MaskProfile::default();
        let t_maskp = if maskp {
            Some(std::time::Instant::now())
        } else {
            None
        };
        for i in 0..mcu_height {
            for (position, component) in self.components.iter_mut().enumerate() {
                if !component.needed {
                    continue;
                }

                // step = 该分量一整行 MCU 的系数个数
                let step = block[position].len() / mcu_height;
                let start = i * step;

                let slice = &block[position][start..start + step];

                // 每行 MCU 宽对应的数据单元数
                let mcu_x = component.width_stride / 8;
                let v_samp = component.vertical_sample;
                // 与 `prog_mask[position]` 对齐的块掩码切片（flat 下标见本函数 doc）。
                let pmask = prog_mask.get(position);

                for k in 0..v_samp {
                    for j in 0..mcu_x {
                        // 反量化（natural-order short wrap，C# `(short)(coeff×q)`）
                        let start = j * 64 + k * 8 * component.width_stride;
                        let qt_slice = match slice.get(start..start + 64) {
                            Some(s) => s,
                            None => {
                                return Err(DecodeError::Format(
                                    "Invalid slice , would panic, invalid image".to_string(),
                                ))
                            }
                        };

                        let t_idct = self.stage.busy_timer();

                        if !StageProfile::skip_idct() {
                            // 环偏移：C# `((m&1)*RowsPerMcu + v*8)*W2 + bx*8`
                            let ring_off = (((i & 1) * v_samp) + k) * 8 * component.width_stride
                                + j * 8;
                            if fused {
                                let flat_blk = (i * v_samp + k) * mcu_x + j;
                                let mask = pmask.and_then(|p| p.get(flat_blk)).copied();
                                if maskp {
                                    pop.accum(mask.unwrap_or(0));
                                }
                                match mask {
                                    // 候选 A：终态 AC 全零 → 免全量 dequant，只算 DC
                                    // （`(coeff[0]*qt16[0]) as i16`，与 dequant_block 截断同义）
                                    // 后走 DC-only（全量 idct 于 AC 全零块逐位恒等，
                                    // dconly_eq_avx2_full_for_ac_zero 已证 → 逐块恒等保持）。
                                    Some(0) => {
                                        // 路由前提：mask==0 ⇒ 终态 AC 系数全零（mask 位 p=zigzag
                                        // 系数序 p，UN_ZIGZAG 双射 → natural 序 1..63 逐位全零）。
                                        // debug_assert 防未来熵段回归静默漏置 mask 位（mask==0 但 AC
                                        // 非零 → 本旁路会错判 DC-only 改输出；release 恒等于零开销）。
                                        debug_assert!(qt_slice[1..].iter().all(|&c| c == 0));
                                        let dc = (i32::from(qt_slice[0])
                                            * i32::from(qt16[position][0]))
                                            as i16;
                                        cs::idct8x8_dc_only(
                                            dc,
                                            &mut component.cs_ring,
                                            ring_off,
                                            component.width_stride,
                                        );
                                    }
                                    // 候选 B：非 DC-only 块走 fused「反量化∘IDCT」（mullo 直喂
                                    // f32 装载，删 128B store+load+栈框；非零块不再算 ac_zero
                                    // movemask 冗余比较）。S4：mask 非零 → last =
                                    // 63−lzcnt(mask)（C# JpegCore.cs:1826 直通），4 态
                                    // 分发到 fused 稀疏核；mask 缺失（边际越界）→ 保守全量，
                                    // 全量 idct 对 DC-only 块亦恒等 → 任意情况字节恒等。
                                    Some(m) => {
                                        if sparse {
                                            let last = 63 - m.leading_zeros() as usize;
                                            dequant_idct8x8_sparse(
                                                qt_slice,
                                                &qt16[position],
                                                last,
                                                &mut component.cs_ring,
                                                ring_off,
                                                component.width_stride,
                                            );
                                        } else {
                                            // 门关（KIMG_JPEG_SPARSE_IDCT=0）：S4 之前的
                                            // 旧 fused 全量核，逐字节一致（同二进制 A/B）。
                                            dequant_idct8x8(
                                                qt_slice,
                                                &qt16[position],
                                                &mut component.cs_ring,
                                                ring_off,
                                                component.width_stride,
                                            );
                                        }
                                    }
                                    None => {
                                        dequant_idct8x8(
                                            qt_slice,
                                            &qt16[position],
                                            &mut component.cs_ring,
                                            ring_off,
                                            component.width_stride,
                                        );
                                    }
                                }
                            } else if fastpath {
                                // 反量化（natural-order short wrap，C# `(short)(coeff×q)`）
                                // + AC 全零判定（终态系数 1..63 全零 → DC-only，对齐 C#
                                // `nz==0/仅DC → last==0 → DcOnly(data[0])`，逐位恒等）。
                                let ac_zero =
                                    dequant_block(qt_slice, &qt16[position], &mut tmp);
                                if ac_zero {
                                    cs::idct8x8_dc_only(
                                        tmp[0],
                                        &mut component.cs_ring,
                                        ring_off,
                                        component.width_stride,
                                    );
                                } else {
                                    cs::idct8x8(
                                        &tmp,
                                        &mut component.cs_ring,
                                        ring_off,
                                        component.width_stride,
                                    );
                                }
                            } else {
                                // 对照路径：向量化反量化后恒全量 IDCT（旧行为）。
                                dequant_block(qt_slice, &qt16[position], &mut tmp);
                                cs::idct8x8(
                                    &tmp,
                                    &mut component.cs_ring,
                                    ring_off,
                                    component.width_stride,
                                );
                            }
                        }
                        self.stage.busy_stop(Stage::Idct, t_idct);
                    }
                }
            }

            self.post_process(pixels, i, width)?;
        }

        // 探针：打印本帧 popcount 分布（env 门控；关联文件名由测量侧解码前打印的
        // `POPMASK FILE <name>` 标记行给出，与 STAGE/SCANP 同款关联口径）。
        if maskp {
            let total_ms = t_maskp.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
            pop.print(total_ms);
        }

        Ok(())
    }
}

/// 渐进 finish 快路径（DC-only 路由）是否启用：`KIMG_JPEG_PROG_FASTPATH` 缺省开，
/// `=0/false/off` 关（仅向量化反量化、恒全量 IDCT）——进程内一次读，供同二进制 A/B（K93）。
#[inline]
fn prog_fastpath_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_JPEG_PROG_FASTPATH").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// 渐进 finish 候选 A+B（本次改动）是否启用：`KIMG_JPEG_FINISH_FUSED` 缺省开，
/// `=0/false/off` 关 → 回退到 HEAD 6de52fe 的旧 finish（`dequant_block` 全量 + ac_zero
/// 路由，即 `KIMG_JPEG_PROG_FASTPATH` 原逻辑）——进程内一次读，供同二进制 A/B（K93）。
#[inline]
fn finish_fused_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_JPEG_FINISH_FUSED").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// S1+S4 稀疏 IDCT 开关（K93 双路径 A/B）：`KIMG_JPEG_SPARSE_IDCT` **默认开**，
/// `=0/false/off` 关 → baseline 稀疏分发（`idct8x8_sparse`）与渐进 finish 的
/// `dequant_idct8x8_sparse` 都走休眠的旧全量核（`idct8x8` / `dequant_idct8x8`），
/// 门关行为与 S1/S4 之前逐字节一致（稀疏 ≡ 全量）——同二进制零代码回退开关。
/// 进程内一次读（OnceLock），调用方在块环外取循环不变布尔，热环内无每块原子开销
/// （同 `dual_sym_enabled` / `finish_fused_enabled` 姿势）。
#[inline]
fn sparse_idct_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let v = std::env::var("KIMG_JPEG_SPARSE_IDCT").unwrap_or_default();
        let t = v.trim();
        !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
    })
}

/// `KIMG_JPEG_PROG_MASK_PROFILE=1`：渐进 finish 逐块 `prog_mask` popcount 分布探针
/// （缺省关=零注入；只在启用时读 env + 计数，不改算法路径/位序）。供候选 C（prog_mask
/// 稀疏 IDCT 分层）取证：DC-only 占比 / 非零 AC ≤4 / 5-16 / >16。
#[inline]
fn popmask_profile_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("KIMG_JPEG_PROG_MASK_PROFILE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    })
}

/// 逐块 popcount 分布累加器（探针读数；只在 `popmask_profile_enabled()` 时构造/打印）。
#[derive(Default)]
struct MaskProfile {
    n: u64,
    dc_only: u64,
    ac_le4: u64,
    ac_516: u64,
    ac_gt16: u64,
}

impl MaskProfile {
    /// 累加一块的 popcount（位 p = zigzag 系数序 p，bit0=DC 恒不置 → popcount=非零 AC 数）。
    #[inline]
    fn accum(&mut self, mask: u64) {
        self.n += 1;
        if mask == 0 {
            self.dc_only += 1;
        } else {
            let pc = u64::from(mask.count_ones());
            if pc <= 4 {
                self.ac_le4 += 1;
            } else if pc <= 16 {
                self.ac_516 += 1;
            } else {
                self.ac_gt16 += 1;
            }
        }
    }

    /// 打印一行 `POPMASK ...`（stdout，与 `SCANP`/`STAGE` 同口径，离线聚合按文件归并）。
    fn print(&self, total_ms: f64) {
        let pct = |v: u64| {
            if self.n > 0 {
                v as f64 / self.n as f64 * 100.0
            } else {
                0.0
            }
        };
        println!(
            "POPMASK n={} dc_only={:.2}% ac_le4={:.2}% ac_516={:.2}% ac_gt16={:.2}% finish_probe_ms={total_ms:.3}",
            self.n,
            pct(self.dc_only),
            pct(self.ac_le4),
            pct(self.ac_516),
            pct(self.ac_gt16),
        );
    }
}

/// 反量化 64 系数块（natural-order `(short)(coeff × q)` 截断 wrap，恒等于旧 i32 标量
/// 与 C# `(short)(coeff[k] * qz[k])`）。顺带返回「AC 全零」判定（终态系数 1..63 全零，
/// 对齐 C# `NonZeroMask → last==0 → DcOnly`），供 DC-only 快路径路由。
/// `coeff.len()` 与 `out` 均须为 64。
#[inline]
fn dequant_block(coeff: &[i16], qt16: &[i16; 64], out: &mut [i16; 64]) -> bool {
    debug_assert_eq!(coeff.len(), 64);
    #[cfg(target_arch = "x86_64")]
    if crate::codecs::jpeg::avx2::avx2_enabled() {
        // SAFETY: 调用方保证 coeff 长度 64；avx2_enabled() 保证 CPU 支持 AVX2。
        return unsafe { dequant_block_avx2(coeff, qt16, out) };
    }
    // 标量：与 AVX2 `_mm256_mullo_epi16` 的低 16 位结果逐位恒等（模 2^16 截断）。
    let mut ac_zero = true;
    for i in 0..64 {
        let v = (i32::from(coeff[i]) * i32::from(qt16[i])) as i16;
        out[i] = v;
        if i != 0 && v != 0 {
            ac_zero = false;
        }
    }
    ac_zero
}

#[cfg(target_arch = "x86_64")]
/// AVX2：向量反量化（`mullo_epi16` = 每 16 位乘积的低 16 位，即 `(short)(coeff×q)` 截断
/// wrap）+ AC 全零判定（索引 1..63 反量化结果全零 → DC-only）。
/// SAFETY: 调用方保证 `coeff.len()==64`；调用方必须已用 `avx2_enabled()` 门控 CPU。
unsafe fn dequant_block_avx2(coeff: &[i16], qt16: &[i16; 64], out: &mut [i16; 64]) -> bool {
    // SAFETY: 长度已知 64；avx2_enabled() 已保证 CPU 支持 AVX2（movemask/mullo/srli 全 AVX2）。
    unsafe {
        use core::arch::x86_64::*;
        let c0 = _mm256_loadu_si256(coeff.as_ptr().add(0) as *const __m256i);
        let c1 = _mm256_loadu_si256(coeff.as_ptr().add(16) as *const __m256i);
        let c2 = _mm256_loadu_si256(coeff.as_ptr().add(32) as *const __m256i);
        let c3 = _mm256_loadu_si256(coeff.as_ptr().add(48) as *const __m256i);
        let q0 = _mm256_loadu_si256(qt16.as_ptr().add(0) as *const __m256i);
        let q1 = _mm256_loadu_si256(qt16.as_ptr().add(16) as *const __m256i);
        let q2 = _mm256_loadu_si256(qt16.as_ptr().add(32) as *const __m256i);
        let q3 = _mm256_loadu_si256(qt16.as_ptr().add(48) as *const __m256i);
        let p0 = _mm256_mullo_epi16(c0, q0);
        let p1 = _mm256_mullo_epi16(c1, q1);
        let p2 = _mm256_mullo_epi16(c2, q2);
        let p3 = _mm256_mullo_epi16(c3, q3);
        _mm256_storeu_si256(out.as_mut_ptr().add(0) as *mut __m256i, p0);
        _mm256_storeu_si256(out.as_mut_ptr().add(16) as *mut __m256i, p1);
        _mm256_storeu_si256(out.as_mut_ptr().add(32) as *mut __m256i, p2);
        _mm256_storeu_si256(out.as_mut_ptr().add(48) as *mut __m256i, p3);
        // AC 全零判定（索引 1..63，p0 掩掉 DC=索引 0）。
        // 注意：① 不得用 `srli_si256` 探测索引 1..15——按 128 位 lane 右移，
        //   索引 8 落 lane 边界被移出而漏检；
        // ② 不得用 `movemask_epi8(all_ac)==0`——它只测每字节最高位，
        //   值如 0x0007 高位置 0 会被漏判；须逐字节 cmp-eq-zero 后全 1。
        let dc_mask = _mm256_setr_epi16(0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1);
        let ac0 = _mm256_and_si256(p0, dc_mask);
        let all_ac = _mm256_or_si256(_mm256_or_si256(ac0, p1), _mm256_or_si256(p2, p3));
        let eq = _mm256_cmpeq_epi8(all_ac, _mm256_setzero_si256());
        _mm256_movemask_epi8(eq) == -1
    }
}

/// 反量化 ∘ IDCT（候选 B fused 分派）：AVX2 → `cs_avx2::dequant_idct8x8_avx2`
/// （mullo 直喂 f32 IDCT，删 128B store+load+栈框）；标量 → `dequant_block` + `cs::idct8x8`
/// （逐位恒等，dequant_avx2_eq_scalar / cs 侧 idct8x8_avx2_eq_scalar 测试骨架已证）。
/// 仅用于「非 DC-only」块（DC-only 由调用方经 prog_mask 预判走 `idct8x8_dc_only`）。
/// `coeff.len()` 须为 64；`out` 以 `(offset, stride)` 写 8×8 字节块。
#[inline]
fn dequant_idct8x8(
    coeff: &[i16],
    qt16: &[i16; 64],
    out: &mut [u8],
    offset: usize,
    stride: usize,
) {
    debug_assert_eq!(coeff.len(), 64);
    #[cfg(target_arch = "x86_64")]
    if crate::codecs::jpeg::avx2::avx2_enabled() {
        // SAFETY: 调用方保证 coeff 长度 64；avx2_enabled() 保证 CPU 支持 AVX2。
        return unsafe {
            crate::codecs::jpeg::cs_avx2::dequant_idct8x8_avx2(
                coeff.as_ptr(),
                qt16.as_ptr(),
                out,
                offset,
                stride,
            )
        };
    }
    let mut tmp = [0_i16; DCT_BLOCK];
    let _ = dequant_block(coeff, qt16, &mut tmp);
    cs::idct8x8(&tmp, out, offset, stride);
}

/// 反量化 ∘ 稀疏 IDCT 分派（S4）：AVX2 → `cs_avx2::dequant_idct8x8_sparse_avx2`
/// （mullo 直喂 f32 稀疏 IDCT）；标量 → `dequant_block` + `cs::idct8x8_sparse_scalar`
/// （逐位恒等）。`last` = 最高非零 zigzag 下标（≥1；==0 的 DC-only 由调用方 prog_mask
/// 预判走 `idct8x8_dc_only`），half_row/half_col 由 `cs::MAX_ROW/MAX_COL` 派生。
/// `coeff.len()` 须为 64；`out` 以 `(offset, stride)` 写 8×8 字节块。
#[inline]
fn dequant_idct8x8_sparse(
    coeff: &[i16],
    qt16: &[i16; 64],
    last: usize,
    out: &mut [u8],
    offset: usize,
    stride: usize,
) {
    debug_assert_eq!(coeff.len(), 64);
    debug_assert!(last < 64);
    let half_rows = cs::MAX_ROW[last.min(79)] < 4;
    let half_cols = cs::MAX_COL[last.min(79)] < 4;
    // 稠密全量态（MAX_ROW/MAX_COL 均 ≥4）：稀疏（全量）≡ 全量 → 直接走 fused 全量核，
    // 免稀疏分发簿记（与 cs::idct8x8_sparse 同款汇聚，K108）。
    if !half_rows && !half_cols {
        dequant_idct8x8(coeff, qt16, out, offset, stride);
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if crate::codecs::jpeg::avx2::avx2_enabled() {
        // SAFETY: 调用方保证 coeff 长度 64；avx2_enabled() 保证 CPU 支持 AVX2。
        return unsafe {
            crate::codecs::jpeg::cs_avx2::dequant_idct8x8_sparse_avx2(
                coeff.as_ptr(),
                qt16.as_ptr(),
                half_rows,
                half_cols,
                out,
                offset,
                stride,
            )
        };
    }
    let mut tmp = [0_i16; DCT_BLOCK];
    let _ = dequant_block(coeff, qt16, &mut tmp);
    cs::idct8x8_sparse_scalar(&tmp, half_rows, half_cols, out, offset, stride);
}

/// 从位流中读取下一个 marker（读取到 marker 或 EOF）。
pub(crate) fn get_marker<'a>(
    reader: &mut ByteReader<'a>,
    stream: &mut BitStream,
) -> Result<Marker, DecodeError> {
    if let Some(marker) = stream.marker {
        stream.marker = None;
        return Ok(marker);
    }

    while !reader.eof()? {
        let marker = reader.read_u8_err()?;

        if marker == 255 {
            let mut r = reader.read_u8_err()?;
            // 0xFF 0xFF...
            while r == 0xFF {
                r = reader.read_u8_err()?;
            }

            if r != 0 {
                return Marker::from_u8(r)
                    .ok_or_else(|| DecodeError::Format(format!("Unknown marker 0xFF{r:X}")));
            }
        }
    }
    Err(DecodeError::ExhaustedData)
}

/// baseline 解码入口（先解析段，再解 MCU）。
pub(crate) fn decode_baseline(
    decoder: &mut Decoder<'_>,
    out_colorspace: ColorSpace,
    pixels: &mut [u8],
) -> Result<(), DecodeError> {
    decoder.out_colorspace = out_colorspace;
    decoder.decode_headers_internal()?;
    let t_total = decoder.stage.timer();
    let r = decoder.decode_mcu_ycbcr_baseline(pixels);
    decoder.stage.stop(Stage::Total, t_total);
    r
}

/// progressive 解码入口（先解析段，再按渐进流程解 MCU）。
pub(crate) fn decode_progressive(
    decoder: &mut Decoder<'_>,
    out_colorspace: ColorSpace,
    pixels: &mut [u8],
) -> Result<(), DecodeError> {
    decoder.out_colorspace = out_colorspace;
    decoder.decode_headers_internal()?;
    let t_total = decoder.stage.timer();
    let r = decoder.decode_mcu_ycbcr_progressive(pixels);
    decoder.stage.stop(Stage::Total, t_total);
    r
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    /// 标量反量化 + 参考 ac_zero（独立实现，避免与被测路径同构）。
    fn ref_scalar(coeff: &[i16; 64], qt16: &[i16; 64]) -> ([i16; 64], bool) {
        let mut out = [0i16; 64];
        let mut ac_zero = true;
        for i in 0..64 {
            let v = (i32::from(coeff[i]) * i32::from(qt16[i])) as i16;
            out[i] = v;
            if i != 0 && v != 0 {
                ac_zero = false;
            }
        }
        (out, ac_zero)
    }

    fn lcg(s: &mut u64) -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s
    }

    #[test]
    fn dequant_avx2_eq_scalar() {
        if !crate::codecs::jpeg::avx2::avx2_enabled() {
            return;
        }
        let mut seed = 0xABCD_1234u64;
        // 单 AC 非零覆盖每一位置（含 128-bit lane 边界 8）——曾因 `srli_si256`
        // 按 lane 右移漏检索引 8 而误判 DC-only。
        for ac_idx in 1..64i32 {
            for dc_mode in [0i16, 12345].into_iter() {
                let mut coeff = [0i16; 64];
                coeff[0] = dc_mode;
                coeff[ac_idx as usize] = (seed as i32 as i16).wrapping_add(12345);
                let mut qt16 = [8i16; 64];
                for q in qt16.iter_mut().skip(1) {
                    *q = if (seed / 97) % 3 == 0 { 200 } else { 19 };
                }
                let mut out_v = [0i16; 64];
                // SAFETY: 测试已门控 AVX2
                let ac_v = unsafe { super::dequant_block_avx2(&coeff, &qt16, &mut out_v) };
                let (ref_out, ref_ac) = ref_scalar(&coeff, &qt16);
                assert!(!ref_ac, "单 AC 非零应 ac_zero=false idx={ac_idx}");
                assert_eq!(out_v, ref_out, "dequant 单 AC 案例 idx={ac_idx} 不恒等");
                assert_eq!(ac_v, ref_ac, "ac_zero 案例 idx={ac_idx} 不恒等");
            }
        }
        for case in 0..4000 {
            let mode = lcg(&mut seed) % 4;
            let mut coeff = [0i16; 64];
            if mode != 2 {
                for i in 0..64 {
                    coeff[i] = match mode {
                        0 => 0i16,
                        1 => {
                            if i == 0 {
                                lcg(&mut seed) as i16
                            } else {
                                0i16
                            }
                        }
                        _ => lcg(&mut seed) as i16,
                    };
                }
            }
            let mut qt16 = [0i16; 64];
            for q in qt16.iter_mut() {
                // 覆盖 8 位表与 16 位表（含 >32767 折叠负）
                *q = match lcg(&mut seed) % 3 {
                    0 => (lcg(&mut seed) % 256) as i16,
                    _ => (lcg(&mut seed) % 65536) as i16,
                };
            }
            let mut out_v = [0i16; 64];
            // SAFETY: 测试已门控 AVX2
            let ac_v = unsafe { super::dequant_block_avx2(&coeff, &qt16, &mut out_v) };
            let (ref_out, ref_ac) = ref_scalar(&coeff, &qt16);
            assert_eq!(out_v, ref_out, "dequant 案例 {case} 不恒等");
            assert_eq!(ac_v, ref_ac, "ac_zero 案例 {case} 不恒等 (mode={mode})");
        }
    }

    /// AC 全零块：全量 cs::idct8x8（AVX2 内核）是否与 cs::idct8x8_dc_only 逐位恒等。
    /// 这是渐进 DC-only 快路径路由的前提；若不等，路由会改输出。
    #[test]
    fn dconly_eq_avx2_full_for_ac_zero() {
        use crate::codecs::jpeg::cs_avx2;
        if !crate::codecs::jpeg::avx2::avx2_enabled() {
            return;
        }
        let mut out_full = [0u8; 64];
        let mut out_dc = [0u8; 64];
        // 全零块
        let data = [0i16; 64];
        // SAFETY: AVX2 已门控
        unsafe { cs_avx2::idct8x8_avx2(&data, &mut out_full, 0, 8) };
        cs::idct8x8_dc_only(0, &mut out_dc, 0, 8);
        assert_eq!(out_full, out_dc, "全零块不恒等");
        // DC-only：密集扫 DC（含反量化后可能的大 DC 范围）
        for dc in (-40000i32..=40000).step_by(1) {
            let mut data = [0i16; 64];
            data[0] = dc as i16;
            let mut out_full = [0u8; 64];
            let mut out_dc = [0u8; 64];
            // SAFETY: AVX2 已门控
            unsafe { cs_avx2::idct8x8_avx2(&data, &mut out_full, 0, 8) };
            cs::idct8x8_dc_only(data[0], &mut out_dc, 0, 8);
            assert_eq!(out_full, out_dc, "DC-only 块不恒等 dc={dc}");
        }
        // 极端 i16 值
        for &dc in &[i16::MIN, i16::MAX, -32768, 32767, -128, 127, -129, 128] {
            let mut data = [0i16; 64];
            data[0] = dc;
            let mut out_full = [0u8; 64];
            let mut out_dc = [0u8; 64];
            // SAFETY: AVX2 已门控
            unsafe { cs_avx2::idct8x8_avx2(&data, &mut out_full, 0, 8) };
            cs::idct8x8_dc_only(dc, &mut out_dc, 0, 8);
            assert_eq!(out_full, out_dc, "DC-only 极端不恒等 dc={dc}");
        }
    }

    /// Fused「反量化∘IDCT」内核是否与「分阶段 dequant_block → idct8x8」逐位恒等。
    /// 随机 500 例 + 边角矩阵（全零/仅 DC/单 AC 逐位/极端值/8 位与 16 位量化表折叠负）。
    /// 结合既有 `dequant_avx2_eq_scalar`（dequant AVX2≡标量）与 cs 侧 `idct8x8_avx2_eq_scalar`
    /// （IDCT AVX2≡标量），链式证「fused ≡ 标量 路径」；标量路径（dequant_block 标量分支 +
    /// cs::idct8x8 标量分支）保持原样不动。
    #[test]
    fn dequant_idct_fused_eq_staged() {
        use crate::codecs::jpeg::cs_avx2;
        if !crate::codecs::jpeg::avx2::avx2_enabled() {
            return;
        }
        let mut seed = 0xDEAD_BEEF_1337u64;
        let check = |coeff: &[i16; 64], qt16: &[i16; 64]| {
            // 分阶段参照：dequant_block_avx2 → idct8x8_avx2（均已被既有测试证 ≡ 标量）。
            let mut tmp = [0i16; 64];
            // SAFETY: 已门控 AVX2
            unsafe { super::dequant_block_avx2(coeff, qt16, &mut tmp) };
            let mut ref_out = [0u8; 64];
            // SAFETY: 已门控 AVX2
            unsafe { cs_avx2::idct8x8_avx2(&tmp, &mut ref_out, 0, 8) };
            // fused：经 mcu 侧分派（avx2 已门控 → cs_avx2::dequant_idct8x8_avx2）。
            let mut fused_out = [0u8; 64];
            super::dequant_idct8x8(coeff, qt16, &mut fused_out, 0, 8);
            assert_eq!(fused_out, ref_out, "fused ≠ staged (dequant∘idct)");
        };
        // 边角矩阵：全零/仅 DC（含极值）、单 AC 逐位（含 128 位 lane 边界 8）。
        for &dc in &[0i16, 1, 12345, i16::MIN, i16::MAX, -32768, 32767] {
            let mut coeff = [0i16; 64];
            coeff[0] = dc;
            let mut qt = [8i16; 64];
            qt[3] = 0;
            check(&coeff, &qt);
        }
        for ac_idx in 1..64i32 {
            let mut coeff = [0i16; 64];
            coeff[0] = 100;
            coeff[ac_idx as usize] = 300;
            check(&coeff, &[8i16; 64]);
        }
        // 随机 500 例（含 16 位量化表折叠负与全零量化表）。
        for _ in 0..500 {
            let mut coeff = [0i16; 64];
            let dense = (lcg(&mut seed) >> 32) % 2 == 0;
            for i in 0..64 {
                coeff[i] = if dense {
                    lcg(&mut seed) as i16
                } else if i == 0 {
                    lcg(&mut seed) as i16
                } else if (lcg(&mut seed) >> 40) % 100 == 0 {
                    lcg(&mut seed) as i16
                } else {
                    0
                };
            }
            let mut qt16 = [0i16; 64];
            for q in qt16.iter_mut() {
                *q = match lcg(&mut seed) % 3 {
                    0 => (lcg(&mut seed) % 256) as i16,
                    _ => (lcg(&mut seed) % 65536) as i16,
                };
            }
            check(&coeff, &qt16);
        }
    }

    /// 稀疏 fused「反量化∘IDCT」内核（S4）≡ 分阶段「dequant_block → 稀疏 IDCT」。
    /// 对随机/单一 AC 逐位/全零/仅 DC 等形态：用 raw 系数最高的非零 zigzag 下标
    /// （与生产 prog_mask 的 last 同口径）派生 half 旗标，断言 mcu 侧 fused 稀疏分派
    /// 与 dequant_block_avx2 + cs 稀疏标量逐位恒等；dequant/稀疏各自已被既有测试
    /// 证 ≡ 标量全量，链式闭合「fused 稀疏 ≡ 标量全量」。
    #[test]
    fn dequant_idct_sparse_fused_eq_staged() {
        use crate::codecs::jpeg::segment::UN_ZIGZAG;
        if !crate::codecs::jpeg::avx2::avx2_enabled() {
            return;
        }
        let zigzag_last = |data: &[i16; 64]| {
            let mut last = 0usize;
            for (n, &v) in data.iter().enumerate() {
                if v != 0 {
                    last = last.max(UN_ZIGZAG.iter().position(|&x| x == n).unwrap_or(0));
                }
            }
            last
        };
        let check = |coeff: &[i16; 64], qt16: &[i16; 64]| {
            let last = zigzag_last(coeff);
            let hr = cs::MAX_ROW[last.min(79)] < 4;
            let hc = cs::MAX_COL[last.min(79)] < 4;
            // 分阶段参照：dequant_block_avx2 → cs::idct8x8_sparse_scalar
            let mut tmp = [0i16; 64];
            // SAFETY: 已门控 AVX2
            unsafe { super::dequant_block_avx2(coeff, qt16, &mut tmp) };
            let mut ref_out = [0u8; 64];
            cs::idct8x8_sparse_scalar(&tmp, hr, hc, &mut ref_out, 0, 8);
            // fused 稀疏：经 mcu 侧分派（avx2 已门控 → cs_avx2::dequant_idct8x8_sparse_avx2）
            let mut fused_out = [0u8; 64];
            super::dequant_idct8x8_sparse(coeff, qt16, last, &mut fused_out, 0, 8);
            assert_eq!(fused_out, ref_out, "fused 稀疏 ≠ staged 稀疏 (last={last}, hr={hr}, hc={hc})");
        };
        // 仅 DC（含极值）→ last=0
        for &dc in &[0i16, 1, 12345, i16::MIN, i16::MAX, -32768, 32767] {
            let mut coeff = [0i16; 64];
            coeff[0] = dc;
            check(&coeff, &[8i16; 64]);
        }
        // 单一 AC 逐位：覆盖双 half（zigzag 0..9）、半列（10..13）、全量（14+）三态
        for ac_idx in 1..64i32 {
            let mut coeff = [0i16; 64];
            coeff[0] = 100;
            coeff[ac_idx as usize] = 300;
            check(&coeff, &[8i16; 64]);
        }
        // 随机（含 16 位量化表折叠负、全零量化表 → mask 位保守但 dequant 归零边角）
        let mut seed = 0xC0FFEE_2026u64;
        for _ in 0..800 {
            let mut coeff = [0i16; 64];
            let dense = (lcg(&mut seed) >> 32) % 2 == 0;
            for i in 0..64 {
                coeff[i] = if dense {
                    lcg(&mut seed) as i16
                } else if i == 0 {
                    lcg(&mut seed) as i16
                } else if (lcg(&mut seed) >> 40) % 100 == 0 {
                    lcg(&mut seed) as i16
                } else {
                    0
                };
            }
            let mut qt16 = [0i16; 64];
            for q in qt16.iter_mut() {
                *q = match lcg(&mut seed) % 3 {
                    0 => (lcg(&mut seed) % 256) as i16,
                    _ => (lcg(&mut seed) % 65536) as i16,
                };
            }
            check(&coeff, &qt16);
        }
    }

    /// prog_mask 的 flat 块序与 finish 平铺块序对齐（探针/DC-only 路由的下标前提）。
    /// 用真实 4:2:0 几何（16×2 MCU 网格）逐字复刻两条代码路径各自的循环，验证：
    /// ① finish 逐块下标 `(i_mcu*v_samp+k)*mcu_x + j` 落在 `prog_mask` 长度内（不越界）；
    /// ② finish 与熵段 AC scan（非交织逐数据单元 `j + row*width_stride/8`）产生的块序序列
    ///    逐位一致且双射（无遗漏/重叠）——即「熵段写哪、finish 读哪」同一块序。
    /// 4:2:0 几何（h_max=2, v_max=2；mcu_x=mcu_y=2）：
    ///   Y： h_samp=2, v_samp=2 → width_stride=2*2*8=32、mcu_x_local=4、块数=2*2*2*2=16；
    ///   Cb/Cr：h_samp=1, v_samp=1 → width_stride=2*1*8=16、mcu_x_local=2、块数=2*1*1*2=4。
    #[test]
    fn prog_mask_flat_order_aligned() {
        let mcu_x_mcu = 2usize; // 2×2 MCU 网格
        let mcu_y = 2usize;
        // (h_samp, v_samp) 真实 4:2:0 三分量
        let comps: [(usize, usize); 3] = [(2, 2), (1, 1), (1, 1)];
        for &(h_samp, v_samp) in &comps {
            let width_stride = mcu_x_mcu * h_samp * 8; // i16 系数字面宽（= 熵段 comp.width_stride）
            let mcu_x = width_stride / 8; // finish 的 mcu_x（= width_stride/8）
            let n_blocks = mcu_x_mcu * h_samp * v_samp * mcu_y;
            // ① finish 逐 MCU 行循环（mcu.rs finish_progressive_decoding 同式）：
            let mut finish_flat: Vec<usize> = Vec::new();
            for i in 0..mcu_y {
                for k in 0..v_samp {
                    for j in 0..mcu_x {
                        let flat = (i * v_samp + k) * mcu_x + j;
                        assert!(flat < n_blocks, "finish flat 越界: flat={flat} n={n_blocks}");
                        finish_flat.push(flat);
                    }
                }
            }
            // ② 熵段非交织 AC scan 逐数据单元循环（bitstream/mcu.rs 同式：
            //    `start = 64*(j + i*(width_stride/8))`，i=数据单元行、j=列）：
            let rows = mcu_y * v_samp; // 数据单元行数
            let cols = width_stride / 8; // 数据单元列数（= mcu_x_local）
            let mut entropy_flat: Vec<usize> = Vec::new();
            for i in 0..rows {
                for j in 0..cols {
                    let start = 64 * (j + i * (width_stride / 8));
                    assert!(start / 64 < n_blocks, "熵段 flat 越界: block={} n={n_blocks}", start / 64);
                    entropy_flat.push(start / 64);
                }
            }
            // finish 与熵段各访问 n_blocks 个不重复块、且序列逐位一致（同一块序）。
            let mut f_sorted = finish_flat.clone();
            f_sorted.sort_unstable();
            f_sorted.dedup();
            assert_eq!(f_sorted.len(), n_blocks, "finish 块序有重叠/遗漏 (v_samp={v_samp})");
            assert_eq!(finish_flat, entropy_flat, "finish 与熵段块序不一致 (v_samp={v_samp})");
            assert_eq!(f_sorted.len(), entropy_flat.len(), "熵段块数不符 (v_samp={v_samp})");
        }
    }
}
