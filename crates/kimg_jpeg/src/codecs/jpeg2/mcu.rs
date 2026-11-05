//! MCU 解码主循环（1:1 复刻 zune-jpeg `mcu.rs`/`mcu_prog.rs` 的 baseline 部分）。

use core::cmp::{max, min};

use crate::codecs::jpeg2::bitstream::BitStream;
use crate::codecs::jpeg2::color::color_convert;
use crate::codecs::jpeg2::error::DecodeError;
use crate::codecs::jpeg2::idct::{idct4x4, idct_int, idct_int_1x1};
use crate::codecs::jpeg2::io::{ByteReader, Marker};
use crate::codecs::jpeg2::segment::{
    calculate_padded_width, ColorSpace, Decoder, SampleRatios, DCT_BLOCK, MAX_COMPONENTS,
};
use crate::codecs::jpeg2::upsampler::{
    upsample, upsample_generic, upsample_horizontal, upsample_hv, upsample_no_op, upsample_vertical,
};

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
                (2, 1) => (SampleRatios::H, upsample_horizontal as _),
                (1, 2) => (SampleRatios::V, upsample_vertical as _),
                (2, 2) => (SampleRatios::HV, upsample_hv as _),
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

        let padded_width = calculate_padded_width(width, self.sample_ratio);

        let mut stream = BitStream::new();
        let mut tmp = [0_i32; DCT_BLOCK];

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

        // 全部分量都在首个 scan 内 → 可直接逐 MCU 后处理；
        // 否则需先缓冲全部分量到 `progressive_mcus`。
        let all_components_in_first_scan = usize::from(self.num_scans) == self.components.len();
        let mut progressive_mcus: [Vec<i16>; 4] = core::array::from_fn(|_| vec![]);

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

        let mut pixels_written = 0;

        let is_hv = usize::from(self.is_interleaved);
        let upsampler_scratch_size = is_hv
            * self
                .components
                .iter()
                .map(|x| x.width_stride)
                .max()
                .unwrap_or(0)
            * 8;
        let mut upsampler_scratch_space = vec![0; upsampler_scratch_size];

        'sos: loop {
            for i in 0..mcu_height {
                if stream.overread_by > 0 {
                    pixels.get_mut(pixels_written..).map(|v| v.fill(128));
                    break;
                }

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

                if all_components_in_first_scan {
                    self.post_process(
                        pixels,
                        i,
                        mcu_height,
                        width,
                        padded_width,
                        &mut pixels_written,
                        &mut upsampler_scratch_space,
                    )?;
                }

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
                        pixels.get_mut(pixels_written..).map(|v| v.fill(128));
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
        block: &[Vec<i16>; MAX_COMPONENTS],
        _mcu_width: usize,
        pixels: &mut [u8],
    ) -> Result<(), DecodeError> {
        let mcu_height = self.mcu_y;

        let is_hv = usize::from(self.is_interleaved);
        let upsampler_scratch_size = is_hv * self.components.first().map(|c| c.width_stride).unwrap_or(0);
        let width = self.width;
        let padded_width = calculate_padded_width(width, self.sample_ratio);

        let mut upsampler_scratch_space = vec![0; upsampler_scratch_size];

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

        let mut pixels_written = 0;

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
                let temp_channel = &mut component.raw_coeff;
                temp_channel[..step].copy_from_slice(slice);
            }

            self.post_process(
                pixels,
                i,
                mcu_height,
                width,
                padded_width,
                &mut pixels_written,
                &mut upsampler_scratch_space,
            )?;
        }

        Ok(())
    }

    /// 解码一行 MCU。
    fn decode_mcu_width<const PROGRESSIVE: bool>(
        &mut self,
        mcu_width: usize,
        mcu_height: usize,
        tmp: &mut [i32; 64],
        stream: &mut BitStream,
        progressive: &mut [Vec<i16>; 4],
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
        tmp: &mut [i32; 64],
        stream: &mut BitStream,
        progressive: &mut [Vec<i16>; 4],
    ) -> Result<McuContinuation, DecodeError> {
        let z_order = self.z_order;
        let z_scans = &z_order[..usize::from(self.num_scans)];

        // 上次 MCU 解码写入了 tmp 的多少：只用于 `fill` 的优化。
        let mut clobber_more_than_4x4 = true;

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

        for j in 0..scan_du_width {
            for &k in z_scans {
                let component = &mut self.components[k];

                let dc_table = self.dc_huffman_tables[component.dc_huff_table % MAX_COMPONENTS]
                    .as_ref()
                    .ok_or_else(|| DecodeError::Format("DC table not found".to_string()))?;

                let ac_table = self.ac_huffman_tables[component.ac_huff_table % MAX_COMPONENTS]
                    .as_ref()
                    .ok_or_else(|| DecodeError::Format("AC table not found".to_string()))?;

                let qt_table = &component.quantization_table;
                let channel = if PROGRESSIVE {
                    let offset =
                        mcu_height * component.width_stride * 8 * component.vertical_sample;
                    let tail = match progressive[k].get_mut(offset..) {
                        Some(t) => t,
                        None => {
                            return Err(DecodeError::Format(
                                "Would panic on slice iteration".to_string(),
                            ))
                        }
                    };
                    tail
                } else {
                    &mut component.raw_coeff
                };

                let component_samples_needed = component.needed;

                let v_step = if SAMPLED && !PROGRESSIVE {
                    0..component.vertical_sample
                } else {
                    0..1
                };

                for v_samp in v_step {
                    let h_step = if SAMPLED && !PROGRESSIVE {
                        0..component.horizontal_sample
                    } else {
                        0..1
                    };

                    for h_samp in h_step {
                        let result = if component_samples_needed {
                            let clobber_len = if !clobber_more_than_4x4 { 32 } else { 64 };

                            tmp[..clobber_len].fill(0);

                            stream.decode_mcu_block(
                                &mut self.reader,
                                dc_table,
                                ac_table,
                                qt_table,
                                tmp,
                                &mut component.dc_pred,
                            )
                        } else {
                            stream.discard_mcu_block(&mut self.reader, dc_table, ac_table)
                        };

                        let len = match result {
                            Ok(len) => len,
                            Err(_e) => {
                                // 非 strict：打印并终止，容忍损坏图像
                                return Ok(McuContinuation::Terminate);
                            }
                        };

                        if component_samples_needed {
                            clobber_more_than_4x4 = len > 10;

                            let idct_position = if PROGRESSIVE {
                                // 非交织：j 直接索引数据单元
                                j * 8
                            } else {
                                let c2 = v_samp * 8;
                                let c3 = ((j * component.horizontal_sample) + h_samp) * 8;

                                component.width_stride * c2 + c3
                            };

                            let idct_pos = match channel.get_mut(idct_position..) {
                                Some(p) => p,
                                None => {
                                    return Err(DecodeError::Format(
                                        "IDCT position out of bounds".to_string(),
                                    ))
                                }
                            };

                            if len <= 1 {
                                idct_int_1x1(tmp, idct_pos, component.width_stride);
                            } else if len <= 10 {
                                idct4x4(tmp, idct_pos, component.width_stride);
                            } else {
                                idct_int(tmp, idct_pos, component.width_stride);
                            }
                        }
                    }
                }
            }

            self.todo = self.todo.wrapping_sub(1);

            if self.todo == 0 {
                self.handle_rst_main(stream)?;
                continue;
            }

            if stream.marker.is_some() && stream.bits_left == 0 {
                break;
            }
        }

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

    /// 逐行后处理：上采样 + 颜色换算写入输出。
    #[allow(clippy::too_many_arguments)]
    fn post_process(
        &mut self,
        pixels: &mut [u8],
        i: usize,
        mcu_height: usize,
        width: usize,
        padded_width: usize,
        pixels_written: &mut usize,
        upsampler_scratch_space: &mut [i16],
    ) -> Result<(), DecodeError> {
        let out_colorspace_components = self.out_colorspace.num_components();

        let mut px = *pixels_written;
        // 是否垂直上采样
        let is_vertically_sampled = self
            .components
            .iter()
            .any(|c| c.sample_ratio == SampleRatios::HV || c.sample_ratio == SampleRatios::V);

        let mut comp_len = self.components.len();

        // YCbCr -> Luma 时只取第一个分量
        if out_colorspace_components < comp_len && self.out_colorspace == ColorSpace::Luma {
            comp_len = out_colorspace_components;
        }
        let input_colorspace = self.input_colorspace;
        let out_colorspace = self.out_colorspace;

        let mut color_conv_function =
            |num_iters: usize, samples: [&[i16]; MAX_COMPONENTS]| -> Result<(), DecodeError> {
                for (pos, output) in pixels[px..]
                    .chunks_exact_mut(width * out_colorspace_components)
                    .take(num_iters)
                    .enumerate()
                {
                    let mut raw_samples: [&[i16]; MAX_COMPONENTS] = [&[], &[], &[], &[]];

                    for (j, samp) in raw_samples.iter_mut().enumerate().take(comp_len) {
                        let temp = samples[j].get(pos * padded_width..(pos + 1) * padded_width);
                        if temp.is_none() {
                            return Err(DecodeError::Format("Missing samples".to_string()));
                        }
                        *samp = temp.unwrap();
                    }
                    color_convert(
                        &raw_samples,
                        input_colorspace,
                        out_colorspace,
                        output,
                        width,
                        padded_width,
                    )?;
                    px += width * out_colorspace_components;
                }
                Ok(())
            };

        let comps = &mut self.components[..];

        if self.is_interleaved && self.out_colorspace != ColorSpace::Luma {
            for comp in comps.iter_mut() {
                upsample(
                    comp,
                    mcu_height,
                    i,
                    upsampler_scratch_space,
                    is_vertically_sampled,
                )?;
            }

            if is_vertically_sampled {
                if i > 0 {
                    // 写出上一行未上采样的行（之前无 row_down）
                    let mut samples: [&[i16]; MAX_COMPONENTS] = [&[], &[], &[], &[]];

                    for (samp, component) in samples.iter_mut().zip(comps.iter()) {
                        *samp = &component.first_row_upsample_dest;
                    }

                    let num_iters = self.coeff * self.v_max;

                    color_conv_function(num_iters, samples)?;
                }

                // 上采样完最后一行后，保存可用于下次上采样的行
                for component in comps.iter_mut() {
                    if component.sample_ratio != SampleRatios::H {
                        let size = component.vertical_sample
                            * component.width_stride
                            * component.sample_ratio.sample();

                        let last_bytes =
                            component.raw_coeff.rchunks_exact_mut(size).next().unwrap();

                        component
                            .first_row_upsample_dest
                            .copy_from_slice(last_bytes);
                    }
                }
            }

            let mut samples: [&[i16]; MAX_COMPONENTS] = [&[], &[], &[], &[]];

            for (samp, component) in samples.iter_mut().zip(comps.iter()) {
                *samp = if component.sample_ratio == SampleRatios::None {
                    &component.raw_coeff
                } else {
                    &component.upsample_dest
                };
            }

            // 垂直采样时除最后一行外少做一次迭代
            let is_last_considered = is_vertically_sampled && (i != mcu_height.saturating_sub(1));

            let num_iters = (8 - usize::from(is_last_considered)) * self.coeff * self.v_max;

            color_conv_function(num_iters, samples)?;
        } else {
            let mut channels_ref: [&[i16]; MAX_COMPONENTS] = [&[]; MAX_COMPONENTS];

            self.components
                .iter()
                .enumerate()
                .for_each(|(pos, x)| channels_ref[pos] = &x.raw_coeff);

            if let SampleRatios::Generic(_, v) = self.sample_ratio {
                color_conv_function(8 * v * self.coeff, channels_ref)?;
            } else {
                color_conv_function(8 * self.coeff, channels_ref)?;
            }
        }

        *pixels_written = px;
        Ok(())
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
    decoder.decode_mcu_ycbcr_baseline(pixels)
}
