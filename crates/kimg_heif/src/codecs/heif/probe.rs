//! `heif_probe`：只做容器 + hvcC SPS（不解码任何 HEVC payload）的 header-only 探测，供 oracle 测试
//! 与门面（`ReadInfo` 等价物）使用。字段与 `target/heif/oracle/*.info.json` 一一对应，便于对拍。

use crate::codecs::heif::container::HeifContainer;
use crate::codecs::heif::error::{HeifError, HeifResult};

/// HEIF header 探测结果（字段名对齐 oracle `info.json`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInfo {
    /// 显示宽度（transform 之后）。
    pub width: i32,
    /// 显示高度（transform 之后）。
    pub height: i32,
    /// 亮度位深（8/10）。
    pub bitdepth: i32,
    /// grid tile 数（非派生图为 1）。
    pub tiles: usize,
    /// SPS 编码宽度（pic_width_in_luma_samples）。
    pub sps_w: i32,
    /// SPS 编码高度。
    pub sps_h: i32,
    /// 亮度位深（SPS）。
    pub bit_y: i32,
    /// 色度位深（SPS）。
    pub bit_c: i32,
    /// chroma_format_idc。
    pub chroma: i32,
    /// general_profile_idc。
    pub profile: i32,
    /// CtbLog2SizeY。
    pub ctb_log2: i32,
    /// 形如 `"{cols}x{rows}"`。
    pub grid: String,
    /// grid 列数。
    pub cols: i32,
    /// grid 行数。
    pub rows: i32,
    /// 是否带 nclx 色彩说明。
    pub nclx: bool,
    /// 是否带 ICC profile。
    pub icc: bool,
    /// transform 属性（clap/irot/imir）数量。
    pub transforms: usize,
    /// conformance window（[left, right, top, bottom]）。
    pub conf: [i32; 4],
}

/// 只解析容器 + SPS（header-only），返回 [`ProbeInfo`]。
pub fn heif_probe(data: &[u8]) -> HeifResult<ProbeInfo> {
    let container = HeifContainer::parse(data)?;
    let sps = container.tile_configs[0].parse_sps().map_err(|_| {
        HeifError::invalid("'hvcC' 里没有可解析的 SPS。")
    })?;
    let (w, h) = container.display_size()?;
    Ok(ProbeInfo {
        width: w,
        height: h,
        bitdepth: sps.bit_depth_y,
        tiles: container.tiles.len(),
        sps_w: sps.width,
        sps_h: sps.height,
        bit_y: sps.bit_depth_y,
        bit_c: sps.bit_depth_c,
        chroma: sps.chroma_format_idc,
        profile: sps.profile_idc,
        ctb_log2: sps.ctb_log2_size_y,
        grid: format!("{}x{}", container.grid_columns, container.grid_rows),
        cols: container.grid_columns,
        rows: container.grid_rows,
        nclx: container.nclx_colour.is_some(),
        icc: container.icc_profile.is_some(),
        transforms: container.transforms.len(),
        conf: [sps.conf_left, sps.conf_right, sps.conf_top, sps.conf_bottom],
    })
}
