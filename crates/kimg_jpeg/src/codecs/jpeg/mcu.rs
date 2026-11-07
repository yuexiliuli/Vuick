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
            comp.setup_upsample_scanline();
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
            // 仅分配需要的分量平面
            if min(
                self.out_colorspace.num_components().saturating_sub(1),
                pos,
            ) == pos
                || comp_len == 4
            {
                // 容量为一整行 MCU 宽
                let len = comp.width_stride * comp.vertical_sample * 8;

                comp.needed = true;
                comp.raw_coeff = vec![0; len];
            } else {
                comp.needed = false;
            }
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

        // 寄存器化位流局部状态：整行 MCU 逐块解码共用一份，免逐块经 `stream` 内存往返
        // （省每块 6 字段快照/回写 + 独立函数调用/寄存器盘存）。
        let mut hot = HotStream::from_stream(stream);

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
        // 块环内按寄存器布尔分支分发（预测恒走、无每符号/每块原子开销；缺省关 = HEAD
        // 单符号路径逐字节一致）。两条路径各自独立函数，互不影响彼此的代码生成。
        let dual = dual_sym_enabled();

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
                            if component_samples_needed {
                                tmp.fill(0);
                            }
                            Ok(64u16)
                        } else if component_samples_needed {
                            // C# `DecodeBlockCore` 每块清零全部 64 系数：不再用「只清首 32」
                            // 的窗口优化——稀疏块会混入上一满块的高位残值，污染全量 f32 IDCT。
                            tmp.fill(0);

                            if dual {
                                hot.decode_mcu_block_dual(
                                    &mut self.reader,
                                    dc_table,
                                    ac_table,
                                    qt_table,
                                    &component.quantization_zq,
                                    tmp,
                                    &mut component.dc_pred,
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
                                )
                            }
                        } else if dual {
                            hot.discard_mcu_block_dual(&mut self.reader, dc_table, ac_table)
                        } else {
                            hot.discard_mcu_block(&mut self.reader, dc_table, ac_table)
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
                                } else {
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
                // 原每 MCU 回写 + RST 处理路径（restart≠0 图像，语义逐字节不变）
                hot.write_back(stream);
                if self.todo == 0 {
                    self.handle_rst_main(stream)?;
                    hot.reload(stream);
                    continue;
                }
            }

            // 免回写：直接查 hot（= 原 write_back 后查 stream，逐字段等价）
            if hot.marker().is_some() && hot.bits_left() == 0 {
                break;
            }
            self.stage.stop(Stage::McuRest, t_mcu);
        }

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
        }

        let mut stream = BitStream::new_progressive(self.succ_low, self.spec_start, self.spec_end);

        // 首个 scan 已被 headers 阶段解析，直接解熵数据
        let result = self.parse_entropy_coded_data(&mut stream, &mut block);

        if result.is_err() {
            // 非 strict：容忍错误，尽量收尾已解出的系数
            return self.finish_progressive_decoding(&block, pixels);
        }

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

                    let result = self.parse_entropy_coded_data(&mut stream, &mut block);

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

        self.finish_progressive_decoding(&block, pixels)
    }

    /// 解单个 scan 的熵编码数据（1:1 复刻 zune `parse_entropy_coded_data`；P1 A′ 改造）。
    #[allow(clippy::too_many_lines, clippy::cast_sign_loss)]
    fn parse_entropy_coded_data(
        &mut self,
        stream: &mut BitStream,
        buffer: &mut [Vec<i16>; MAX_COMPONENTS],
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
                                    ab,
                                    bl,
                                )?
                            } else {
                                stream.decode_mcu_ac_refine_v(
                                    &mut self.reader,
                                    unsafe { &*ac_ptr },
                                    data,
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

            // 交织 DC scan：逐 MCU 逐分量解码（仅 DC 允许交织）
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
                                if self.succ_high == 0 {
                                    stream.decode_prog_dc_first(
                                        &mut self.reader,
                                        huff_table,
                                        data,
                                        &mut component.dc_pred,
                                    )?;
                                } else {
                                    stream.decode_prog_dc_refine(&mut self.reader, data)?;
                                }
                                self.stage.busy_stop(Stage::Entropy, t_entropy);
                                if self.stage.counting() {
                                    self.stage.n_entropy += 1;
                                }
                            }
                        }
                    }

                    self.todo = self.todo.wrapping_sub(1);

                    self.handle_rst_main(stream)?;
                }
            }
        }

        Ok(())
    }

    /// 渐进收尾：整帧反量化 → f32 IDCT → 后处理（C# `EmitCoefficients` 同款）。
    #[allow(clippy::too_many_lines)]
    fn finish_progressive_decoding(
        &mut self,
        block: &[Vec<i16>; MAX_COMPONENTS],
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
            // 仅分配需要的分量平面
            if min(
                self.out_colorspace.num_components().saturating_sub(1),
                pos,
            ) == pos
                || self.input_colorspace == ColorSpace::YCCK
                || self.input_colorspace == ColorSpace::CMYK
            {
                comp.needed = true;
                comp.raw_coeff = vec![0; comp.width_stride * comp.vertical_sample * 8];
            } else {
                comp.needed = false;
            }
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
        // A/B 门控（进程内一次）：`KIMG_JPEG_PROG_FASTPATH=0` → 仅向量化反量化、恒全量
        // IDCT（旧路径），缺省 → DC-only 快路径路由。同二进制 A/B（K93 姿势）。
        let fastpath = prog_fastpath_enabled();
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

                for k in 0..component.vertical_sample {
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
                            let ring_off =
                                (((i & 1) * component.vertical_sample) + k) * 8 * component.width_stride
                                    + j * 8;
                            if fastpath {
                                // 反量化（natural-order short wrap，C# `(short)(coeff×q)`）
                                // + AC 全零判定（终态系数 1..63 全零 → DC-only，对齐 C#
                                // `nz==0/仅DC → last==0 → DcOnly(data[0])`，逐位恒等）。
                                let ac_zero = dequant_block(qt_slice, &qt16[position], &mut tmp);
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
}
