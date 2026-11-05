//! JPEG 段/表结构解析与解码器状态（1:1 复刻 zune-jpeg `headers.rs`/`misc.rs`/`components.rs`/
//! `decoder.rs::decode_headers_internal` 的解析语义）。

use core::cmp::max;

use crate::codecs::jpeg2::error::DecodeError;
use crate::codecs::jpeg2::huffman::HuffmanTable;
use crate::codecs::jpeg2::io::{ByteReader, Marker};
use crate::codecs::jpeg2::upsampler::{upsample_no_op, UpSampler};

pub(crate) const MAX_COMPONENTS: usize = 4;
pub(crate) const DCT_BLOCK: usize = 64;

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
        })
    }

    /// 分配上采样所需的工作缓冲区。
    pub(crate) fn setup_upsample_scanline(&mut self) {
        self.row = vec![0; self.width_stride * self.vertical_sample];
        self.row_up = vec![0; self.width_stride * self.vertical_sample];
        self.first_row_upsample_dest =
            vec![128; self.vertical_sample * self.width_stride * self.sample_ratio.sample()];
        self.upsample_dest =
            vec![0; self.width_stride * self.sample_ratio.sample() * self.fix_an_annoying_bug * 8];
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
    pub(crate) headers_decoded: bool,
    pub(crate) seen_sof: bool,
    pub(crate) icc_data: Vec<ICCChunk>,
    pub(crate) coeff: usize,
    pub(crate) sample_ratio: SampleRatios,
    pub(crate) exif_data: Option<Vec<u8>>,
    pub(crate) xmp_data: Option<Vec<u8>>,
    pub(crate) iptc_data: Option<Vec<u8>>,
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
            headers_decoded: false,
            seen_sof: false,
            icc_data: vec![],
            coeff: 1,
            sample_ratio: SampleRatios::None,
            exif_data: None,
            xmp_data: None,
            iptc_data: None,
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

/// 计算按 16/8 对齐的填充宽度。
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
