use super::*;

// ---------------------------------------------------------------------------
// V1：帧头解析
// ---------------------------------------------------------------------------

/// 段特性（RFC 6386 §10）。
#[derive(Debug, Clone, Default)]
pub(crate) struct SegmentFeatures {
    pub enabled: bool,
    pub update_map: bool,
    pub absolute: bool,
    pub quant: [i16; 4],
    pub filter: [i16; 4],
    pub map_probabilities: [u8; 3],
}

/// 环路滤波参数（RFC 6386 §14.4）。
#[derive(Debug, Clone, Default)]
pub(crate) struct FilterSettings {
    pub simple: bool,
    pub level: u8,
    pub sharpness: u8,
    pub delta_enabled: bool,
    pub ref_deltas: [i8; 4],
    pub mode_deltas: [i8; 4],
}

/// 量化参数（RFC 6386 §14.1）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Quantizers {
    pub base: i32,
    pub y_dc_delta: i32,
    pub y2_dc_delta: i32,
    pub y2_ac_delta: i32,
    pub uv_dc_delta: i32,
    pub uv_ac_delta: i32,
}

/// 每个段的解量化因子与环路滤波阈值（每帧一次计算）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SegmentParameters {
    pub y_dc: i32,
    pub y_ac: i32,
    pub y2_dc: i32,
    pub y2_ac: i32,
    pub uv_dc: i32,
    pub uv_ac: i32,
    pub filter_level_i16: u8,
    pub filter_level_i4: u8,
}

/// 解析完毕的帧头 + 分区（V1 产物；V2 起由 `decode_vp8` 消费）。
#[derive(Debug, Clone)]
pub(crate) struct FrameHeader<'a> {
    pub width: u32,
    pub height: u32,
    pub mb_width: usize,
    pub mb_height: usize,
    pub segments: SegmentFeatures,
    pub filter: FilterSettings,
    pub quantizers: Quantizers,
    /// 系数概率（4*8*3*11，eob/zero/one... 11 节点）。
    pub coefficient_probabilities: [u8; 4 * 8 * 3 * 11],
    /// skip 标志是否存在。
    pub skip_flag_present: bool,
    /// skip 概率（不存在时为 0）。
    pub skip_probability: u8,
    /// 宏块数据分区（数量 = partition_count）。
    pub partitions: Vec<&'a [u8]>,
    /// 帧头解码器（宏块头模式继续消费；保留给 V2）。
    pub header: Vp8BoolDecoder<'a>,
}

/// 解析完整 VP8 keyframe 帧头与 token 分区表。
pub(crate) fn parse_frame_header(frame: &[u8]) -> Result<FrameHeader<'_>> {
    if frame.len() < 10 {
        return Err(Vp8Error::new("VP8 keyframe 头被截断"));
    }
    let frame_tag = u32::from(frame[0]) | (u32::from(frame[1]) << 8) | (u32::from(frame[2]) << 16);
    if (frame_tag & 1) != 0 {
        return Err(Vp8Error::new("VP8 帧间预测（非 keyframe）暂不支持"));
    }
    let partition_length = (frame_tag >> 5) as usize;
    if partition_length > frame.len() - 10 || frame[3] != 0x9d || frame[4] != 0x01 || frame[5] != 0x2a {
        return Err(Vp8Error::new("VP8 keyframe 头或首分区非法"));
    }
    let coded_width = u32::from(u16::from_le_bytes([frame[6], frame[7]])) & 0x3fff;
    let coded_height = u32::from(u16::from_le_bytes([frame[8], frame[9]])) & 0x3fff;
    if coded_width == 0 || coded_height == 0 {
        return Err(Vp8Error::new("VP8 尺寸为零"));
    }
    if u64::from(coded_width) * u64::from(coded_height) > MAX_PIXELS {
        return Err(Vp8Error::new("VP8 像素超限"));
    }

    let header_partition = &frame[10..10 + partition_length];
    let mut header = Vp8BoolDecoder::new(header_partition);
    if header.read_bit(128) != 0 {
        return Err(Vp8Error::new("VP8 保留色空间标志非零"));
    }
    let _ = header.read_bit(128);

    let segments = read_segments(&mut header)?;
    let filter = read_filter(&mut header)?;
    let partition_count = 1usize << header.read_literal(2) as usize;
    let quantizers = read_quantizers(&mut header)?;
    // 保留位（RFC 6386 §19.1 之后、系数概率之前）。
    let _ = header.read_bit(128);
    let mut coefficient_probabilities = [128u8; 4 * 8 * 3 * 11];
    coefficient_probabilities.copy_from_slice(&DEFAULT_COEFF_PROBS);
    read_coefficient_updates(&mut header, &mut coefficient_probabilities)?;
    let skip_flag_present = header.read_bit(128) != 0;
    let skip_probability = if skip_flag_present {
        header.read_literal(8) as u8
    } else {
        0
    };

    let token_frame = &frame[10 + partition_length..];
    let partitions = read_partitions(token_frame, partition_count)?;

    Ok(FrameHeader {
        width: coded_width,
        height: coded_height,
        mb_width: coded_width.div_ceil(16) as usize,
        mb_height: coded_height.div_ceil(16) as usize,
        segments,
        filter,
        quantizers,
        coefficient_probabilities,
        skip_flag_present,
        skip_probability,
        partitions,
        header,
    })
}

/// RFC 6386 §19.3 段特性。
fn read_segments(d: &mut Vp8BoolDecoder<'_>) -> Result<SegmentFeatures> {
    let enabled = d.read_bit(128) != 0;
    if !enabled {
        return Ok(SegmentFeatures::default());
    }
    let update_map = d.read_bit(128) != 0;
    let update_data = d.read_bit(128) != 0;
    let mut s = SegmentFeatures {
        enabled,
        update_map,
        absolute: false,
        quant: [0; 4],
        filter: [0; 4],
        map_probabilities: [255; 3],
    };
    if update_data {
        s.absolute = d.read_bit(128) != 0;
        for q in s.quant.iter_mut() {
            if d.read_bit(128) != 0 {
                *q = d.read_signed_literal(7) as i16;
            }
        }
        for f in s.filter.iter_mut() {
            if d.read_bit(128) != 0 {
                *f = d.read_signed_literal(6) as i16;
            }
        }
    }
    if update_map {
        for p in s.map_probabilities.iter_mut() {
            if d.read_bit(128) != 0 {
                *p = d.read_literal(8) as u8;
            }
        }
    }
    Ok(s)
}

/// RFC 6386 §19.3 环路滤波。
fn read_filter(d: &mut Vp8BoolDecoder<'_>) -> Result<FilterSettings> {
    let simple = d.read_bit(128) != 0;
    let level = d.read_literal(6) as u8;
    let sharpness = d.read_literal(3) as u8;
    let mut f = FilterSettings {
        simple,
        level,
        sharpness,
        delta_enabled: false,
        ref_deltas: [0; 4],
        mode_deltas: [0; 4],
    };
    let delta_enabled = d.read_bit(128) != 0;
    if delta_enabled && d.read_bit(128) != 0 {
        f.delta_enabled = true;
        for r in f.ref_deltas.iter_mut() {
            if d.read_bit(128) != 0 {
                *r = d.read_signed_literal(6) as i8;
            }
        }
        for m in f.mode_deltas.iter_mut() {
            if d.read_bit(128) != 0 {
                *m = d.read_signed_literal(6) as i8;
            }
        }
    }
    Ok(f)
}

/// RFC 6386 §19.3 量化索引 + 各 delta。
fn read_quantizers(d: &mut Vp8BoolDecoder<'_>) -> Result<Quantizers> {
    let base = d.read_literal(7);
    let read_delta = |d: &mut Vp8BoolDecoder<'_>| -> i32 {
        if d.read_bit(128) == 0 {
            0
        } else {
            d.read_signed_literal(4)
        }
    };
    Ok(Quantizers {
        base,
        y_dc_delta: read_delta(d),
        y2_dc_delta: read_delta(d),
        y2_ac_delta: read_delta(d),
        uv_dc_delta: read_delta(d),
        uv_ac_delta: read_delta(d),
    })
}

/// RFC 6386 §13.4 系数概率更新（默认表 + 按扑克概率逐节点替换）。
fn read_coefficient_updates(d: &mut Vp8BoolDecoder<'_>, probabilities: &mut [u8; 4 * 8 * 3 * 11]) -> Result<()> {
    for (update_index, slot) in probabilities.iter_mut().enumerate() {
        if d.read_bit(UPDATE_COEFF_PROBS[update_index]) != 0 {
            *slot = d.read_literal(8) as u8;
        }
    }
    Ok(())
}

/// RFC 6386 §19.4 token 分区表：前 (count-1) 个分区带 3 字节尺寸，最后分区取余量。
fn read_partitions(data: &[u8], count: usize) -> Result<Vec<&[u8]>> {
    if count > 8 {
        return Err(Vp8Error::new("VP8 分区数超限"));
    }
    let size_count = count - 1;
    let table_length = size_count * 3;
    if data.len() < table_length {
        return Err(Vp8Error::new("VP8 token 分区尺寸表被截断"));
    }
    let mut sizes = [0usize; 7];
    for (i, size) in sizes[..size_count].iter_mut().enumerate() {
        let off = i * 3;
        *size = data[off] as usize | (data[off + 1] as usize) << 8 | (data[off + 2] as usize) << 16;
    }
    let mut offset = table_length;
    let mut parts = Vec::with_capacity(count);
    for (i, &stored) in sizes[..count].iter().enumerate() {
        let size = if i == size_count {
            data.len() - offset
        } else {
            stored
        };
        if size > data.len() - offset {
            return Err(Vp8Error::new("VP8 token 分区超出帧"));
        }
        parts.push(&data[offset..offset + size]);
        offset += size;
    }
    Ok(parts)
}

/// 每段解量化因子与滤波阈值（RFC 6386 §14.1/§19.3 段 delta 融合）。
pub(crate) fn compute_segment_parameters(
    segments: &SegmentFeatures,
    quantizers: &Quantizers,
    filter: &FilterSettings,
) -> [SegmentParameters; 4] {
    let mut out = [SegmentParameters::default(); 4];
    for (segment, p) in out.iter_mut().enumerate() {
        let segment_quant = if segments.enabled {
            i32::from(segments.quant[segment])
        } else {
            0
        };
        let q_index = if segments.enabled && segments.absolute {
            segment_quant
        } else {
            quantizers.base + segment_quant
        }
        .clamp(0, 127);
        let segment_filter = if segments.enabled {
            i32::from(segments.filter[segment])
        } else {
            0
        };
        let filter_level = if segments.enabled && segments.absolute {
            segment_filter
        } else {
            i32::from(filter.level) + segment_filter
        };
        let mut level_i16 = filter_level;
        let mut level_i4 = filter_level;
        if filter.delta_enabled {
            level_i16 += i32::from(filter.ref_deltas[0]);
            level_i4 += i32::from(filter.ref_deltas[0]) + i32::from(filter.mode_deltas[0]);
        }
        p.filter_level_i16 = level_i16.clamp(0, 63) as u8;
        p.filter_level_i4 = level_i4.clamp(0, 63) as u8;
        p.y_dc = i32::from(DC_QUANTIZERS[(q_index + quantizers.y_dc_delta).clamp(0, 127) as usize]);
        p.y_ac = i32::from(AC_QUANTIZERS[q_index as usize]);
        p.y2_dc = 2 * i32::from(DC_QUANTIZERS[(q_index + quantizers.y2_dc_delta).clamp(0, 127) as usize]);
        p.y2_ac =
            (155 * i32::from(AC_QUANTIZERS[(q_index + quantizers.y2_ac_delta).clamp(0, 127) as usize]) / 100)
                .max(8);
        p.uv_dc =
            i32::from(DC_QUANTIZERS[(q_index + quantizers.uv_dc_delta).clamp(0, 127) as usize]).min(132);
        p.uv_ac = i32::from(AC_QUANTIZERS[(q_index + quantizers.uv_ac_delta).clamp(0, 127) as usize]);
    }
    out
}

