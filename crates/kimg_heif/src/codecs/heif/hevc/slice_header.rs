//! 帧内解码器需要的切片段头字段（H.265 §7.3.6.1）。移植自 C# `Hevc\HevcSliceHeader.cs`（全量）。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::bit_reader::HevcBitReader;
use crate::codecs::heif::hevc::parameter_sets::{HevcPps, HevcSps};

#[derive(Debug, Clone, Default)]
pub struct HevcSliceHeader {
    pub first_slice_segment_in_pic: bool,
    pub dependent_slice_segment: bool,
    pub segment_address: i32,

    /// SliceAddrRs：本 segment 所属（独立）切片的第一个 CTB 的地址。
    pub slice_addr_rs: i32,

    pub slice_type: i32,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub slice_qp_y: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,

    /// slice_segment_data() 在去转义 RBSP 里的字节偏移。
    pub data_offset: usize,

    /// entry_point_offset_minus1[i] + 1：转义 NAL 单元里除最后一个外每个子流的字节数。
    pub entry_point_offsets: Vec<i64>,
}

impl HevcSliceHeader {
    pub const SliceTypeB: i32 = 0;
    pub const SliceTypeP: i32 = 1;
    pub const SliceTypeI: i32 = 2;

    /// 解析切片段头。`previous` 是同图画里前一个切片段的头，依赖切片段要继承它的字段。
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        rbsp: &[u8],
        nal_type: u8,
        sps: &HevcSps,
        pps: &HevcPps,
        previous: Option<&HevcSliceHeader>,
    ) -> HeifResult<HevcSliceHeader> {
        let mut r = HevcBitReader::new(rbsp, 2);
        let first_slice_segment_in_pic = r.read_flag()?;
        if (16..=23).contains(&nal_type) {
            r.skip(1)?; // no_output_of_prior_pics_flag
        }
        let pps_id = r.read_ue_max(63, "slice_pic_parameter_set_id")?;
        if pps_id != pps.id {
            return Err(HeifError::invalid(
                "切片引用了一个不在图像解码器配置里的 PPS。",
            ));
        }

        let pic_size_in_ctbs = sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs();
        let mut h = HevcSliceHeader {
            first_slice_segment_in_pic,
            dependent_slice_segment: false,
            segment_address: 0,
            slice_addr_rs: 0,
            slice_type: HevcSliceHeader::SliceTypeI,
            sao_luma: false,
            sao_chroma: false,
            slice_qp_y: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            deblocking_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            loop_filter_across_slices: false,
            data_offset: 0,
            entry_point_offsets: Vec::new(),
        };
        if !h.first_slice_segment_in_pic {
            if pps.dependent_slice_segments_enabled {
                h.dependent_slice_segment = r.read_flag()?;
            }
            let bits = 32 - (pic_size_in_ctbs as u32 - 1).leading_zeros() as i64;
            h.segment_address = r.read_bits(bits as usize)? as i32;
            if h.segment_address >= pic_size_in_ctbs {
                return Err(HeifError::invalid("slice_segment_address 越界。"));
            }
        }

        if h.dependent_slice_segment {
            let previous = previous.ok_or_else(|| {
                HeifError::invalid("依赖切片段没有前一个切片段。")
            })?;
            h.slice_addr_rs = previous.slice_addr_rs;
            h.slice_type = previous.slice_type;
            h.sao_luma = previous.sao_luma;
            h.sao_chroma = previous.sao_chroma;
            h.slice_qp_y = previous.slice_qp_y;
            h.cb_qp_offset = previous.cb_qp_offset;
            h.cr_qp_offset = previous.cr_qp_offset;
            h.deblocking_disabled = previous.deblocking_disabled;
            h.beta_offset_div2 = previous.beta_offset_div2;
            h.tc_offset_div2 = previous.tc_offset_div2;
            h.loop_filter_across_slices = previous.loop_filter_across_slices;
        } else {
            h.slice_addr_rs = h.segment_address;
            r.skip(pps.num_extra_slice_header_bits as i64)?;
            h.slice_type = r.read_ue_max(2, "slice_type")?;
            if h.slice_type != HevcSliceHeader::SliceTypeI {
                return Err(HeifError::invalid(
                    "HEIF 图像 item 里只支持帧内 (I) 切片。",
                ));
            }
            if pps.output_flag_present {
                r.skip(1)?;
            }
            if nal_type != 19 && nal_type != 20 {
                // 不是 IDR：slice_pic_order_cnt_lsb 与参考图画集。CRA 或 trailing 图画的 I 切片
                // 也照报；它们不影响帧内解码，所以跳过。
                r.skip(sps.log2_max_poc_lsb as i64)?;
                if !r.read_flag()? {
                    Self::skip_short_term_rps(&mut r, sps.num_short_term_ref_pic_sets)?;
                } else if sps.num_short_term_ref_pic_sets > 1 {
                    let bits = 32 - (sps.num_short_term_ref_pic_sets as u32 - 1).leading_zeros() as i64;
                    r.skip(bits)?;
                }
                if sps.long_term_ref_pics_present {
                    let num_lt_sps = if sps.num_long_term_ref_pics_sps > 0 {
                        r.read_ue_max(sps.num_long_term_ref_pics_sps as u32, "num_long_term_sps")?
                    } else {
                        0
                    };
                    let num_lt_pics = r.read_ue_max(32, "num_long_term_pics")?;
                    for i in 0..num_lt_sps + num_lt_pics {
                        if i < num_lt_sps {
                            if sps.num_long_term_ref_pics_sps > 1 {
                                r.skip((32 - (sps.num_long_term_ref_pics_sps as u32 - 1).leading_zeros()) as i64)?;
                            }
                        } else {
                            r.skip(sps.log2_max_poc_lsb as i64 + 1)?;
                        }
                        if r.read_flag()? {
                            r.read_ue()?;
                        }
                    }
                }
                if sps.temporal_mvp_enabled {
                    r.skip(1)?;
                }
            }
            if sps.sao_enabled {
                h.sao_luma = r.read_flag()?;
                if sps.chroma_format_idc != 0 {
                    h.sao_chroma = r.read_flag()?;
                }
            }

            let qp_bd_offset_y = 6 * (sps.bit_depth_y - 8);
            h.slice_qp_y = pps.init_qp + r.read_se_range(-87, 77, "slice_qp_delta")?;
            if h.slice_qp_y < -qp_bd_offset_y || h.slice_qp_y > 51 {
                return Err(HeifError::invalid("SliceQpY 越界。"));
            }
            if pps.slice_chroma_qp_offsets_present {
                h.cb_qp_offset = r.read_se_range(-12, 12, "slice_cb_qp_offset")?;
                h.cr_qp_offset = r.read_se_range(-12, 12, "slice_cr_qp_offset")?;
            }

            let override_flag = pps.deblocking_filter_override_enabled && r.read_flag()?;
            h.deblocking_disabled = pps.deblocking_filter_disabled;
            h.beta_offset_div2 = pps.beta_offset_div2;
            h.tc_offset_div2 = pps.tc_offset_div2;
            if override_flag {
                h.deblocking_disabled = r.read_flag()?;
                if !h.deblocking_disabled {
                    h.beta_offset_div2 = r.read_se_range(-6, 6, "slice_beta_offset_div2")?;
                    h.tc_offset_div2 = r.read_se_range(-6, 6, "slice_tc_offset_div2")?;
                }
            }
            h.loop_filter_across_slices = pps.loop_filter_across_slices_enabled;
            if pps.loop_filter_across_slices_enabled
                && (h.sao_luma || h.sao_chroma || !h.deblocking_disabled)
            {
                h.loop_filter_across_slices = r.read_flag()?;
            }
        }

        if pps.tiles_enabled || pps.entropy_coding_sync_enabled {
            // 串行解码器不需要入口点（子流字节对齐地依次跟排，CABAC 引擎跟踪它的位位置）。
            let count = r.read_ue_max(pic_size_in_ctbs as u32, "num_entry_point_offsets")? as usize;
            if count > 0 {
                let offset_bits = r.read_ue_max(31, "offset_len_minus1")? as usize + 1;
                h.entry_point_offsets = Vec::with_capacity(count);
                for _ in 0..count {
                    let mut value: i64 = 0;
                    for _ in 0..offset_bits {
                        value = (value << 1) | i64::from(r.read_bit()?);
                    }
                    h.entry_point_offsets.push(value + 1);
                }
            }
        }
        if pps.slice_header_extension_present {
            let length = r.read_ue_max(256, "slice_segment_header_extension_length")?;
            r.skip(length as i64 * 8)?;
        }

        // byte_alignment()：一个 1 位，然后 0 位补齐到字节边界。
        if r.read_bit()? != 1 {
            return Err(HeifError::invalid("切片头的 byte_alignment() 畸形。"));
        }
        while !r.byte_aligned() {
            r.skip(1)?;
        }
        h.data_offset = r.byte_position();
        Ok(h)
    }

    fn skip_short_term_rps(
        r: &mut HevcBitReader<'_>,
        num_sets: i32,
    ) -> HeifResult<()> {
        // 切片的 st_ref_pic_set(num_short_term_ref_pic_sets)。帧间 RPS 预测需要 SPS 集的尺寸；
        // 那些只对帧间图画出现，这里的预测集直接拒绝。
        if num_sets != 0 && r.read_flag()? {
            return Err(HeifError::invalid(
                "帧间预测参考图画集不受支持。",
            ));
        }
        let negative = r.read_ue_max(16, "num_negative_pics")?;
        let positive = r.read_ue_max(16, "num_positive_pics")?;
        for _ in 0..negative + positive {
            r.read_ue()?;
            r.skip(1)?;
        }
        Ok(())
    }
}
