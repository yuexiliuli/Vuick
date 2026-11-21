//! HEVC 参数集：`HevcScan`（扫描序）、`HevcScalingList`（缩放表）、`HevcSps`（序列参数集）、
//! `HevcPps`（图像参数集）。移植自 C# `Hevc\HevcParameterSets.cs`（字段/分支一比一）。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::bit_reader::HevcBitReader;

/// H.265 §6.5.3–6.5.5 的扫描序数组（块尺寸 1、2、4、8）。
pub struct HevcScan;

impl HevcScan {
    /// `[log2BlockSize 0..3][scanIdx 0..2]` → 打包位置，x 在低半字节、y 在高半字节。
    pub fn order() -> Vec<Vec<Vec<u8>>> {
        let mut result = Vec::with_capacity(4);
        for log2 in 0..4usize {
            let size = 1usize << log2;
            let mut diag = vec![0u8; size * size];
            let mut hor = vec![0u8; size * size];
            let mut ver = vec![0u8; size * size];

            // 6.5.3 右上对角线。
            let mut i = 0usize;
            let mut x = 0i32;
            let mut y = 0i32;
            while i < size * size {
                while y >= 0 {
                    if (x as usize) < size && (y as usize) < size {
                        diag[i] = (x as u8) | ((y as u8) << 4);
                        i += 1;
                    }
                    y -= 1;
                    x += 1;
                }
                y = x;
                x = 0;
            }

            i = 0;
            for yy in 0..size as u8 {
                for xx in 0..size as u8 {
                    hor[i] = xx | (yy << 4);
                    i += 1;
                }
            }
            i = 0;
            for xx in 0..size as u8 {
                for yy in 0..size as u8 {
                    ver[i] = xx | (yy << 4);
                    i += 1;
                }
            }
            result.push(vec![diag, hor, ver]);
        }
        result
    }
}

/// §7.4.5 的缩放因子 m[x][y]，按 (sizeId, matrixId) 展开成完整块尺寸，行主序（下标 y * size + x）。
#[derive(Debug, Clone)]
pub struct HevcScalingList {
    pub factors: Vec<Vec<Vec<u8>>>,
}

const DEFAULT_INTRA8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19,
    20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35,
    35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];

const DEFAULT_INTER8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20,
    20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28,
    28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

impl HevcScalingList {
    /// Table 7-5 / 7-6 的默认表。
    pub fn create_default() -> HevcScalingList {
        let mut lists: Vec<Vec<Vec<u8>>> = Vec::with_capacity(4);
        let mut dc: Vec<Vec<i32>> = Vec::with_capacity(4);
        for size_id in 0..4usize {
            let mut per_matrix = Vec::with_capacity(6);
            let mut per_dc = Vec::with_capacity(6);
            for matrix_id in 0..6 {
                per_matrix.push(if size_id == 0 {
                    vec![16u8; 16]
                } else {
                    if matrix_id < 3 {
                        DEFAULT_INTRA8.to_vec()
                    } else {
                        DEFAULT_INTER8.to_vec()
                    }
                });
                per_dc.push(16);
            }
            lists.push(per_matrix);
            dc.push(per_dc);
        }
        Self::expand(&lists, &dc)
    }

    /// scaling_list_data() 的 §7.3.4。
    pub fn parse(r: &mut HevcBitReader<'_>) -> HeifResult<HevcScalingList> {
        let (defaults, default_dc) = Self::create_default_lists();
        let mut lists: Vec<Vec<Vec<u8>>> = Vec::with_capacity(4);
        let mut dc: Vec<Vec<i32>> = Vec::with_capacity(4);
        for (size_id, default_lists) in defaults.iter().enumerate() {
            // C# 预分配 byte[6][] / int[6]；Rust 用预置 6 槽（size_id==3 时矩阵只写 0、3，
            // 1/2/4/5 由下方补表填），保证下标恒在界内。
            let mut per_matrix = vec![Vec::new(); 6];
            let mut per_dc = vec![0i32; 6];
            let coef_num = (64usize).min(1usize << (4 + (size_id << 1)));
            let mut matrix_id = 0usize;
            while matrix_id < 6 {
                let pred_mode_flag = r.read_flag()?;
                if !pred_mode_flag {
                    let max_delta = if size_id == 3 { (matrix_id / 3) as u32 } else { matrix_id as u32 };
                    let delta = r.read_ue_max(max_delta, "scaling_list_pred_matrix_id_delta")?;
                    if delta == 0 {
                        per_matrix[matrix_id] = default_lists[matrix_id].clone();
                        per_dc[matrix_id] = default_dc;
                    } else {
                        let ref_matrix_id = matrix_id - (delta as usize) * (if size_id == 3 { 3 } else { 1 });
                        per_matrix[matrix_id] = per_matrix[ref_matrix_id].clone();
                        per_dc[matrix_id] = per_dc[ref_matrix_id];
                    }
                } else {
                    let mut next_coef: i32 = 8;
                    if size_id > 1 {
                        next_coef = r.read_se_range(-7, 247, "scaling_list_dc_coef_minus8")? + 8;
                        per_dc[matrix_id] = next_coef;
                    }
                    let mut list = vec![0u8; coef_num];
                    for e in list.iter_mut() {
                        let delta_coef = r.read_se_range(-128, 127, "scaling_list_delta_coef")?;
                        next_coef = (next_coef + delta_coef + 256) % 256;
                        if next_coef == 0 {
                            return Err(HeifError::invalid("缩放表条目为零。"));
                        }
                        *e = next_coef as u8;
                    }
                    per_matrix[matrix_id] = list;
                }
                matrix_id += if size_id == 3 { 3 } else { 1 };
            }
            lists.push(per_matrix);
            dc.push(per_dc);
        }

        // 32x32 色度矩阵（1、2、4、5）只对 4:4:4 有意义，4:4:4 不解码；给 16x16 的表让表完备。
        for matrix_id in 0..6usize {
            if matrix_id % 3 == 0 {
                continue;
            }
            lists[3][matrix_id] = lists[2][matrix_id].clone();
            dc[3][matrix_id] = dc[2][matrix_id];
        }
        Ok(Self::expand(&lists, &dc))
    }

    fn create_default_lists() -> (Vec<Vec<Vec<u8>>>, i32) {
        let dc: i32 = 16;
        let mut lists: Vec<Vec<Vec<u8>>> = Vec::with_capacity(4);
        for size_id in 0..4usize {
            let mut per_matrix = Vec::with_capacity(6);
            for matrix_id in 0..6 {
                per_matrix.push(if size_id == 0 {
                    vec![16u8; 16]
                } else if matrix_id < 3 {
                    DEFAULT_INTRA8.to_vec()
                } else {
                    DEFAULT_INTER8.to_vec()
                });
            }
            lists.push(per_matrix);
        }
        (lists, dc)
    }

    fn expand(lists: &[Vec<Vec<u8>>], dc: &[Vec<i32>]) -> HevcScalingList {
        let scan2 = &HevcScan::order()[2][0];
        let scan3 = &HevcScan::order()[3][0];
        let mut factors: Vec<Vec<Vec<u8>>> = Vec::with_capacity(4);
        for size_id in 0..4usize {
            let size = 4usize << size_id;
            let mut per_matrix = Vec::with_capacity(6);
            for matrix_id in 0..6 {
                let mut factor = vec![0u8; size * size];
                let list = &lists[size_id][matrix_id];
                if size_id == 0 {
                    for i in 0..16usize {
                        let x = usize::from(scan2[i] & 15);
                        let y = usize::from(scan2[i]) >> 4;
                        factor[y * 4 + x] = list[i];
                    }
                } else {
                    // 8x8 表按 1、2 或 4 上采样（7-45, 7-46, 7-48）。
                    let ratio = size / 8;
                    for i in 0..64usize {
                        let x = usize::from(scan3[i] & 15);
                        let y = usize::from(scan3[i]) >> 4;
                        for j in 0..ratio {
                            for k in 0..ratio {
                                factor[(y * ratio + j) * size + x * ratio + k] = list[i];
                            }
                        }
                    }
                    if size_id >= 2 {
                        factor[0] = dc[size_id][matrix_id] as u8;
                    }
                }
                per_matrix.push(factor);
            }
            factors.push(per_matrix);
        }
        HevcScalingList { factors }
    }
}

/// 帧内解码器要用的序列参数集字段（H.265 §7.3.2.2）。
#[derive(Debug, Clone)]
pub struct HevcSps {
    pub id: i32,
    pub profile_idc: i32,
    /// general_profile_compatibility_flag[j]，位于位 31 - j。
    pub profile_compatibility: u32,
    pub chroma_format_idc: i32,
    pub width: i32,
    pub height: i32,
    /// 亮度采样里的 conformance window。
    pub conf_left: i32,
    pub conf_right: i32,
    pub conf_top: i32,
    pub conf_bottom: i32,
    pub bit_depth_y: i32,
    pub bit_depth_c: i32,
    pub log2_max_poc_lsb: i32,
    pub min_cb_log2_size_y: i32,
    pub ctb_log2_size_y: i32,
    pub min_tb_log2_size_y: i32,
    pub max_tb_log2_size_y: i32,
    pub max_transform_hierarchy_depth_intra: i32,
    pub scaling_list_enabled: bool,
    pub scaling_list: Option<HevcScalingList>,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm_bit_depth_y: i32,
    pub pcm_bit_depth_c: i32,
    pub log2_min_pcm_cb_size_y: i32,
    pub log2_max_pcm_cb_size_y: i32,
    pub pcm_loop_filter_disabled: bool,
    pub num_short_term_ref_pic_sets: i32,
    pub long_term_ref_pics_present: bool,
    pub num_long_term_ref_pics_sps: i32,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing_enabled: bool,

    // VUI 视频信号类型（E.2.1）；缺省按 VUI 语义取默认值。
    pub video_full_range: bool,
    pub colour_primaries: i32,
    pub transfer_characteristics: i32,
    pub matrix_coefficients: i32,

    // VUI chroma_sample_loc_type_top_field（E.2.1）；缺省/在 VUI 结束前读到 → 0（左）。
    pub chroma_sample_loc_type: i32,
}

impl HevcSps {
    pub fn ctb_size_y(&self) -> i32 {
        1 << self.ctb_log2_size_y
    }
    pub fn pic_width_in_ctbs(&self) -> i32 {
        (self.width + self.ctb_size_y() - 1) >> self.ctb_log2_size_y
    }
    pub fn pic_height_in_ctbs(&self) -> i32 {
        (self.height + self.ctb_size_y() - 1) >> self.ctb_log2_size_y
    }

    /// 本解码器单边接受的最大图画尺寸；level 6.2 允许 8192 × 4320。
    pub const MaxDimension: u32 = 16_888;

    pub fn parse(rbsp: &[u8]) -> HeifResult<HevcSps> {
        let mut r = HevcBitReader::new(rbsp, 2); // 跳过 2 字节 NAL 头
        let mut sps = HevcSps {
            id: 0,
            profile_idc: 0,
            profile_compatibility: 0,
            chroma_format_idc: 0,
            width: 0,
            height: 0,
            conf_left: 0,
            conf_right: 0,
            conf_top: 0,
            conf_bottom: 0,
            bit_depth_y: 0,
            bit_depth_c: 0,
            log2_max_poc_lsb: 0,
            min_cb_log2_size_y: 0,
            ctb_log2_size_y: 0,
            min_tb_log2_size_y: 0,
            max_tb_log2_size_y: 0,
            max_transform_hierarchy_depth_intra: 0,
            scaling_list_enabled: false,
            scaling_list: None,
            amp_enabled: false,
            sao_enabled: false,
            pcm_enabled: false,
            pcm_bit_depth_y: 0,
            pcm_bit_depth_c: 0,
            log2_min_pcm_cb_size_y: 0,
            log2_max_pcm_cb_size_y: 0,
            pcm_loop_filter_disabled: false,
            num_short_term_ref_pic_sets: 0,
            long_term_ref_pics_present: false,
            num_long_term_ref_pics_sps: 0,
            temporal_mvp_enabled: false,
            strong_intra_smoothing_enabled: false,
            video_full_range: false,
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            chroma_sample_loc_type: 0,
        };
        r.skip(4)?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = r.read_bits(3)? as i32;
        if max_sub_layers_minus1 > 6 {
            return Err(HeifError::invalid("sps_max_sub_layers_minus1 越界。"));
        }
        r.skip(1)?; // sps_temporal_id_nesting_flag
        let (profile_idc, profile_compat) = Self::profile_tier_level(&mut r, max_sub_layers_minus1)?;
        sps.profile_idc = profile_idc;
        sps.profile_compatibility = profile_compat;
        if !Self::is_supported_profile(sps.profile_idc, sps.profile_compatibility) {
            return Err(HeifError::invalid(format!(
                "HEVC general_profile_idc {} 不受支持；只支持 Main (1)、Main 10 (2) 与 Main Still Picture (3)。",
                sps.profile_idc
            )));
        }
        sps.id = r.read_ue_max(15, "sps_seq_parameter_set_id")?;
        sps.chroma_format_idc = r.read_ue_max(3, "chroma_format_idc")?;
        if sps.chroma_format_idc == 3 && r.read_flag()? {
            return Err(HeifError::invalid("separate_colour_plane_flag 不受支持。"));
        }
        sps.width = r.read_ue_max(Self::MaxDimension, "pic_width_in_luma_samples")?;
        sps.height = r.read_ue_max(Self::MaxDimension, "pic_height_in_luma_samples")?;
        if sps.width == 0 || sps.height == 0 {
            return Err(HeifError::invalid("HEVC 图尺寸为零。"));
        }

        let sub_width = if sps.chroma_format_idc == 1 || sps.chroma_format_idc == 2 { 2 } else { 1 };
        let sub_height = if sps.chroma_format_idc == 1 { 2 } else { 1 };
        if r.read_flag()? {
            sps.conf_left = r.read_ue_max(Self::MaxDimension, "conf_win_left_offset")? * sub_width;
            sps.conf_right = r.read_ue_max(Self::MaxDimension, "conf_win_right_offset")? * sub_width;
            sps.conf_top = r.read_ue_max(Self::MaxDimension, "conf_win_top_offset")? * sub_height;
            sps.conf_bottom = r.read_ue_max(Self::MaxDimension, "conf_win_bottom_offset")? * sub_height;
            if sps.conf_left + sps.conf_right >= sps.width || sps.conf_top + sps.conf_bottom >= sps.height {
                return Err(HeifError::invalid("conformance window 为空。"));
            }
        }

        sps.bit_depth_y = 8 + r.read_ue_max(8, "bit_depth_luma_minus8")?;
        sps.bit_depth_c = 8 + r.read_ue_max(8, "bit_depth_chroma_minus8")?;
        sps.log2_max_poc_lsb = 4 + r.read_ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")?;
        let sub_layer_ordering_info = r.read_flag()?;
        let start = if sub_layer_ordering_info { 0 } else { max_sub_layers_minus1 };
        for _ in start..=max_sub_layers_minus1 {
            r.read_ue()?;
            r.read_ue()?;
            r.read_ue()?;
        }

        sps.min_cb_log2_size_y = 3 + r.read_ue_max(3, "log2_min_luma_coding_block_size_minus3")?;
        sps.ctb_log2_size_y = sps.min_cb_log2_size_y + r.read_ue_max(3, "log2_diff_max_min_luma_coding_block_size")?;
        sps.min_tb_log2_size_y = 2 + r.read_ue_max(3, "log2_min_luma_transform_block_size_minus2")?;
        sps.max_tb_log2_size_y = sps.min_tb_log2_size_y + r.read_ue_max(3, "log2_diff_max_min_luma_transform_block_size")?;
        if sps.ctb_log2_size_y < 4
            || sps.ctb_log2_size_y > 6
            || sps.min_tb_log2_size_y >= sps.min_cb_log2_size_y
            || sps.max_tb_log2_size_y > sps.ctb_log2_size_y.min(5)
        {
            return Err(HeifError::invalid("HEVC 编码/变换块尺寸越界。"));
        }
        // 7.4.3.2.1：图画尺寸是 MinCbSizeY 的倍数，所以每个编码块都在平面内。
        if sps.width % (1 << sps.min_cb_log2_size_y) != 0
            || sps.height % (1 << sps.min_cb_log2_size_y) != 0
        {
            return Err(HeifError::invalid(
                "HEVC 图尺寸不是最小编码块尺寸的倍数。",
            ));
        }
        r.read_ue()?; // max_transform_hierarchy_depth_inter
        sps.max_transform_hierarchy_depth_intra = r.read_ue_max(
            (sps.ctb_log2_size_y - sps.min_tb_log2_size_y) as u32,
            "max_transform_hierarchy_depth_intra",
        )?;

        sps.scaling_list_enabled = r.read_flag()?;
        if sps.scaling_list_enabled {
            sps.scaling_list = Some(if r.read_flag()? {
                HevcScalingList::parse(&mut r)?
            } else {
                HevcScalingList::create_default()
            });
        }
        sps.amp_enabled = r.read_flag()?;
        sps.sao_enabled = r.read_flag()?;
        sps.pcm_enabled = r.read_flag()?;
        if sps.pcm_enabled {
            sps.pcm_bit_depth_y = 1 + r.read_bits(4)? as i32;
            sps.pcm_bit_depth_c = 1 + r.read_bits(4)? as i32;
            sps.log2_min_pcm_cb_size_y = 3 + r.read_ue_max(2, "log2_min_pcm_luma_coding_block_size_minus3")?;
            sps.log2_max_pcm_cb_size_y = sps.log2_min_pcm_cb_size_y + r.read_ue_max(2, "log2_diff_max_min_pcm_luma_coding_block_size")?;
            sps.pcm_loop_filter_disabled = r.read_flag()?;
            if sps.pcm_bit_depth_y > sps.bit_depth_y
                || sps.pcm_bit_depth_c > sps.bit_depth_c
                || sps.log2_max_pcm_cb_size_y > sps.ctb_log2_size_y.min(5)
            {
                return Err(HeifError::invalid("HEVC PCM 参数越界。"));
            }
        }

        sps.num_short_term_ref_pic_sets = r.read_ue_max(64, "num_short_term_ref_pic_sets")?;
        let mut delta_poc_counts = vec![0i32; sps.num_short_term_ref_pic_sets as usize + 1];
        for i in 0..sps.num_short_term_ref_pic_sets as usize {
            delta_poc_counts[i] = Self::skip_short_term_ref_pic_set(
                &mut r,
                i,
                sps.num_short_term_ref_pic_sets as usize,
                &delta_poc_counts,
            )?;
        }
        sps.long_term_ref_pics_present = r.read_flag()?;
        if sps.long_term_ref_pics_present {
            sps.num_long_term_ref_pics_sps = r.read_ue_max(32, "num_long_term_ref_pics_sps")?;
            for _ in 0..sps.num_long_term_ref_pics_sps {
                r.skip(sps.log2_max_poc_lsb as i64)?;
                r.skip(1)?;
            }
        }
        sps.temporal_mvp_enabled = r.read_flag()?;
        sps.strong_intra_smoothing_enabled = r.read_flag()?;
        if r.read_flag()? {
            Self::parse_vui_signal_type(&mut r, &mut sps)?;
        }

        // sps_extension：range / multilayer / 3D / SCC 扩展都在 Main / Main 10 之外；合规流在
        // VUI 色彩描述之后就把 sps_range_extension_flag 等留在 0，这里不需再解析。
        Ok(sps)
    }

    /// profile_tier_level(1, maxNumSubLayersMinus1)（§7.3.3）；返回 general_profile_idc。
    /// Main、Main 10 与 Main Still Picture（A.3.2–A.3.4）：profile_idc 1/2/3，或 profile_idc 0 时
    /// 看兼容标志集。其余（profile_idc ≥ 4 的 range 扩展、SCC、multilayer）用本解码器未实现的工具。
    pub fn is_supported_profile(profile_idc: i32, compatibility: u32) -> bool {
        if matches!(profile_idc, 1..=3) {
            return true;
        }
        if profile_idc != 0 {
            return false;
        }
        const MAIN: u32 = 1u32 << (31 - 1);
        const MAIN10: u32 = 1u32 << (31 - 2);
        const STILL_PICTURE: u32 = 1u32 << (31 - 3);
        compatibility & (MAIN | MAIN10 | STILL_PICTURE) != 0
    }

    pub fn profile_tier_level(
        r: &mut HevcBitReader<'_>,
        max_sub_layers_minus1: i32,
    ) -> HeifResult<(i32, u32)> {
        // General profile：profile_space(2) + tier(1) + profile_idc(5) + 32 兼容标志 + 4 源标志 +
        // 43 constraint 位 + 1 inbld/保留位 + general_level_idc(8)：共 96 位。
        r.skip(3)?;
        let profile_idc = r.read_bits(5)? as i32;
        let compatibility = r.read_bits(32)?;
        r.skip(4 + 43 + 1 + 8)?;
        let mut profile_present = [false; 8];
        let mut level_present = [false; 8];
        for i in 0..max_sub_layers_minus1 as usize {
            profile_present[i] = r.read_flag()?;
            level_present[i] = r.read_flag()?;
        }
        if max_sub_layers_minus1 > 0 {
            for _i in max_sub_layers_minus1 as usize..8 {
                r.skip(2)?;
            }
        }
        for i in 0..max_sub_layers_minus1 as usize {
            if profile_present[i] {
                r.skip(88)?;
            }
            if level_present[i] {
                r.skip(8)?;
            }
        }
        Ok((profile_idc, compatibility))
    }

    /// st_ref_pic_set(stRpsIdx)（§7.3.7）跳过；返回 NumDeltaPocs 供后续帧间预测。
    fn skip_short_term_ref_pic_set(
        r: &mut HevcBitReader<'_>,
        index: usize,
        count: usize,
        delta_poc_counts: &[i32],
    ) -> HeifResult<i32> {
        let inter_prediction = index != 0 && r.read_flag()?;
        if inter_prediction {
            let mut delta_idx = 1usize;
            if index == count {
                delta_idx = r.read_ue_max(index as u32, "delta_idx_minus1")? as usize + 1;
            }
            r.skip(1)?; // delta_rps_sign
            r.read_ue()?; // abs_delta_rps_minus1
            let ref_index = index as i64 - delta_idx as i64;
            if ref_index < 0 {
                return Err(HeifError::invalid("short-term RPS 引用非法。"));
            }
            let mut total = 0i32;
            for _ in 0..=delta_poc_counts[ref_index as usize] {
                let used = r.read_flag()?;
                let use_delta = used || r.read_flag()?;
                if use_delta {
                    total += 1;
                }
            }
            return Ok(total);
        }

        let negative = r.read_ue_max(16, "num_negative_pics")?;
        let positive = r.read_ue_max(16, "num_positive_pics")?;
        for _ in 0..negative + positive {
            r.read_ue()?;
            r.skip(1)?;
        }
        Ok(negative + positive)
    }

    fn parse_vui_signal_type(r: &mut HevcBitReader<'_>, sps: &mut HevcSps) -> HeifResult<()> {
        if r.read_flag()? {
            // aspect_ratio_info_present_flag
            if r.read_bits(8)? == 255 {
                r.skip(32)?; // EXTENDED_SAR: sar_width, sar_height
            }
        }
        if r.read_flag()? {
            r.skip(1)?; // overscan_info_present_flag → overscan_appropriate_flag
        }
        if r.read_flag()? {
            // video_signal_type_present_flag
            r.skip(3)?; // video_format
            sps.video_full_range = r.read_flag()?;
            if r.read_flag()? {
                // colour_description_present_flag
                sps.colour_primaries = r.read_bits(8)? as i32;
                sps.transfer_characteristics = r.read_bits(8)? as i32;
                sps.matrix_coefficients = r.read_bits(8)? as i32;
            }
        }

        // chroma_loc_info_present_flag + 两个 ue(v)（top/bottom field）。它只是信息性的（平面输出
        // 报告它；RGB 转换无论怎样都复制色度），所以 VUI 到此为止、或值在标准定义 0..5 之外时读成
        // 默认 0，而不让今天能解码的图画失败。
        if r.bits_left() < 1 || !r.read_flag()? {
            return Ok(());
        }
        let mut probe = r.clone();
        if let Ok(top) = probe.read_ue() {
            sps.chroma_sample_loc_type = if top <= 5 { top as i32 } else { 0 };
        } else {
            sps.chroma_sample_loc_type = 0;
        }
        Ok(())
    }
}

/// 帧内解码器要用的图像参数集字段（H.265 §7.3.2.3）。
#[derive(Debug, Clone)]
pub struct HevcPps {
    pub id: i32,
    pub sps_id: i32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: i32,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub init_qp: i32, // 26 + init_qp_minus26
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub loop_filter_across_slices_enabled: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub scaling_list: Option<HevcScalingList>,
    pub lists_modification_present: bool,
    pub slice_header_extension_present: bool,
    pub log2_max_transform_skip_size: i32,
}

impl HevcPps {
    pub fn parse(rbsp: &[u8]) -> HeifResult<HevcPps> {
        let mut r = HevcBitReader::new(rbsp, 2);
        let mut pps = HevcPps {
            id: r.read_ue_max(63, "pps_pic_parameter_set_id")?,
            sps_id: r.read_ue_max(15, "pps_seq_parameter_set_id")?,
            dependent_slice_segments_enabled: false,
            output_flag_present: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_enabled: false,
            cabac_init_present: false,
            init_qp: 0,
            constrained_intra_pred: false,
            transform_skip_enabled: false,
            cu_qp_delta_enabled: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present: false,
            transquant_bypass_enabled: false,
            tiles_enabled: false,
            entropy_coding_sync_enabled: false,
            loop_filter_across_slices_enabled: false,
            deblocking_filter_override_enabled: false,
            deblocking_filter_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            scaling_list: None,
            lists_modification_present: false,
            slice_header_extension_present: false,
            log2_max_transform_skip_size: 2,
        };
        pps.dependent_slice_segments_enabled = r.read_flag()?;
        pps.output_flag_present = r.read_flag()?;
        pps.num_extra_slice_header_bits = r.read_bits(3)? as i32;
        pps.sign_data_hiding_enabled = r.read_flag()?;
        pps.cabac_init_present = r.read_flag()?;
        r.read_ue()?; // num_ref_idx_l0_default_active_minus1
        r.read_ue()?; // num_ref_idx_l1_default_active_minus1
        pps.init_qp = 26 + r.read_se_range(-(26 + 6 * 8), 25, "init_qp_minus26")?;
        pps.constrained_intra_pred = r.read_flag()?;
        pps.transform_skip_enabled = r.read_flag()?;
        pps.cu_qp_delta_enabled = r.read_flag()?;
        if pps.cu_qp_delta_enabled {
            pps.diff_cu_qp_delta_depth = r.read_ue_max(3, "diff_cu_qp_delta_depth")?;
        }
        pps.cb_qp_offset = r.read_se_range(-12, 12, "pps_cb_qp_offset")?;
        pps.cr_qp_offset = r.read_se_range(-12, 12, "pps_cr_qp_offset")?;
        pps.slice_chroma_qp_offsets_present = r.read_flag()?;
        r.skip(2)?; // weighted_pred_flag, weighted_bipred_flag
        pps.transquant_bypass_enabled = r.read_flag()?;
        pps.tiles_enabled = r.read_flag()?;
        pps.entropy_coding_sync_enabled = r.read_flag()?;
        if pps.tiles_enabled {
            // 解析以便正确读取 PPS 其余部分；解码 tile 由切片解码器拒绝。
            let columns = r.read_ue_max(255, "num_tile_columns_minus1")?;
            let rows = r.read_ue_max(255, "num_tile_rows_minus1")?;
            if !r.read_flag()? {
                for _ in 0..columns {
                    r.read_ue()?;
                }
                for _ in 0..rows {
                    r.read_ue()?;
                }
            }
            r.skip(1)?; // loop_filter_across_tiles_enabled_flag
        }
        pps.loop_filter_across_slices_enabled = r.read_flag()?;
        if r.read_flag()? {
            // deblocking_filter_control_present_flag
            pps.deblocking_filter_override_enabled = r.read_flag()?;
            pps.deblocking_filter_disabled = r.read_flag()?;
            if !pps.deblocking_filter_disabled {
                pps.beta_offset_div2 = r.read_se_range(-6, 6, "pps_beta_offset_div2")?;
                pps.tc_offset_div2 = r.read_se_range(-6, 6, "pps_tc_offset_div2")?;
            }
        }
        if r.read_flag()? {
            pps.scaling_list = Some(HevcScalingList::parse(&mut r)?);
        }
        pps.lists_modification_present = r.read_flag()?;
        r.read_ue()?; // log2_parallel_merge_level_minus2
        pps.slice_header_extension_present = r.read_flag()?;
        if r.read_flag()? {
            // pps_extension_present_flag
            let range_extension = r.read_flag()?;
            r.skip(7)?; // multilayer, 3d, scc, 4 bits
            if range_extension {
                // pps_range_extension()：对 Main / Main 10 只有开头的 transform-skip 尺寸有意义；
                // 其余任何非默认值都是本解码器未实现的 range-extension 工具。
                if pps.transform_skip_enabled {
                    pps.log2_max_transform_skip_size =
                        2 + r.read_ue_max(3, "log2_max_transform_skip_block_size_minus2")?;
                }
                if r.read_flag()? {
                    return Err(HeifError::invalid(
                        "cross_component_prediction 不受支持（HEVC range extensions）。",
                    ));
                }
                if r.read_flag()? {
                    return Err(HeifError::invalid(
                        "chroma_qp_offset_list 不受支持（HEVC range extensions）。",
                    ));
                }
                if r.read_ue()? != 0 || r.read_ue()? != 0 {
                    return Err(HeifError::invalid(
                        "SAO 偏移缩放不受支持（HEVC range extensions）。",
                    ));
                }
            }
        }
        Ok(pps)
    }
}
