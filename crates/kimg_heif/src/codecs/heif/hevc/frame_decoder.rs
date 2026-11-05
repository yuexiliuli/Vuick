//! 帧解码器（H.265 §7.3.8）：**本阶段编译绿的 stub**。切片数据解析/CTU 遍历/CU 语法/位读取
//! 有待后续 cabac+intra+transform+lf+sao 阶段按序移植 `Hevc\HevcFrameDecoder.cs`。
//! 构造函数保留 C# 的支持范围校验（tile/PCM/色度/位深）——它们定义「这个解码器能接什么」。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::parameter_sets::{HevcPps, HevcSps};
use crate::codecs::heif::hevc::picture::HevcPicture;

/// 每 CTB 的 SAO 参数（C# 同名字步类型，供后续阶段使用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SaoParams {
    pub type_y: u8,
    pub type_cb: u8,
    pub type_cr: u8,
    pub band_y: u8,
    pub band_cb: u8,
    pub band_cr: u8,
    pub class_y: u8,
    pub class_cb: u8,
    pub class_cr: u8,
    /// SaoOffsetVal[c][1..4]。
    pub o: [i8; 12],
}

impl SaoParams {
    pub fn ty(&self, c: usize) -> u8 {
        match c {
            0 => self.type_y,
            1 => self.type_cb,
            _ => self.type_cr,
        }
    }
    pub fn band(&self, c: usize) -> u8 {
        match c {
            0 => self.band_y,
            1 => self.band_cb,
            _ => self.band_cr,
        }
    }
    pub fn class_(&self, c: usize) -> u8 {
        match c {
            0 => self.class_y,
            1 => self.class_cb,
            _ => self.class_cr,
        }
    }
    pub fn offset(&self, c: usize, i: usize) -> i8 {
        self.o[(c * 4 + i).min(11)]
    }
}

/// 解码一个帧内 HEVC 图画（其全部切片段）为 [`HevcPicture`]。
pub struct HevcFrameDecoder {
    pub sps: HevcSps,
    pub pps: HevcPps,
    pub picture: HevcPicture,
    _log2_min_cu_qp_delta_size: i32,
}

impl HevcFrameDecoder {
    pub fn new(
        sps: HevcSps,
        pps: HevcPps,
        _pooled: bool,
        _parallel: bool,
    ) -> HeifResult<HevcFrameDecoder> {
        if pps.tiles_enabled {
            return Err(HeifError::unsupported("HEVC tiles 不受支持。"));
        }
        if sps.pcm_enabled {
            return Err(HeifError::unsupported("HEVC PCM 编码不受支持。"));
        }
        if sps.chroma_format_idc != 1 {
            return Err(HeifError::unsupported(format!(
                "HEVC chroma_format_idc {} 不受支持；只支持 4:2:0。",
                sps.chroma_format_idc
            )));
        }
        if sps.bit_depth_y > 10 || sps.bit_depth_c > 10 {
            return Err(HeifError::unsupported("HEVC 位深超过 10 不受支持。"));
        }
        let log2_min_cu_qp_delta_size = sps.ctb_log2_size_y - pps.diff_cu_qp_delta_depth;
        if log2_min_cu_qp_delta_size < sps.min_cb_log2_size_y {
            return Err(HeifError::invalid("diff_cu_qp_delta_depth 越界。"));
        }
        let mut picture = HevcPicture::new(
            sps.width,
            sps.height,
            sps.chroma_format_idc,
            sps.bit_depth_y,
            sps.bit_depth_c,
        );
        picture.crop_left = sps.conf_left;
        picture.crop_right = sps.conf_right;
        picture.crop_top = sps.conf_top;
        picture.crop_bottom = sps.conf_bottom;

        Ok(HevcFrameDecoder {
            sps,
            pps,
            picture,
            _log2_min_cu_qp_delta_size: log2_min_cu_qp_delta_size,
        })
    }

    /// 解码 `(nal_type, 去转义 RBSP)` 切片。**未实现**（后续阶段）。
    pub fn decode(&mut self, _slices: Vec<(u8, Vec<u8>)>) -> HeifResult<HevcPicture> {
        Err(HeifError::not_implemented(
            "HEVC 帧解码（CABAC/CTU）尚未实现。",
        ))
    }
}
