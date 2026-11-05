//! HEVC 解码器配置记录（ISO/IEC 14496-15 §8.3.3，`hvcC` 属性）：NAL 长度前缀位宽 + 参数集 NAL
//! 单元。移植自 C# `Hevc\HevcImageDecoder.cs` 顶部的 `HevcDecoderConfiguration`。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::nal::Nal;
use crate::codecs::heif::hevc::parameter_sets::HevcSps;

/// 一条记录内参数集 NAL 单元的硬上限。
const MaxNalUnits: usize = 256;

/// 一条 HEVC 解码器配置（`hvcC`）：`length_size` 位宽的 NAL 长度前缀 + 全部参数集 NAL 单元。
#[derive(Debug, Clone)]
pub struct HevcDecoderConfiguration {
    pub length_size: usize,
    pub nal_units: Vec<Vec<u8>>,
}

impl HevcDecoderConfiguration {
    /// 解析 `hvcC` box。
    pub fn parse(hvcc: &[u8]) -> HeifResult<HevcDecoderConfiguration> {
        // 22 个固定字节（configurationVersion .. lengthSizeMinusOne）+ numOfArrays。
        if hvcc.len() < 23 {
            return Err(HeifError::invalid("'hvcC' box 被截断。"));
        }
        if hvcc[0] != 1 {
            return Err(HeifError::invalid(format!(
                "不支持的 'hvcC' configurationVersion {}。",
                hvcc[0]
            )));
        }
        let length_size = usize::from(hvcc[21] & 3) + 1;
        if length_size == 3 {
            return Err(HeifError::invalid("'hvcC' 的 NAL 长度宽度为 3 字节，不允许。"));
        }

        let arrays = hvcc[22];
        let mut p = 23usize;
        let mut nal_units = Vec::new();
        for _ in 0..arrays {
            if p + 3 > hvcc.len() {
                return Err(HeifError::invalid("'hvcC' box 被截断。"));
            }
            let count = (u16::from(hvcc[p + 1]) << 8) | u16::from(hvcc[p + 2]);
            p += 3;
            for _ in 0..count {
                if p + 2 > hvcc.len() {
                    return Err(HeifError::invalid("'hvcC' box 被截断。"));
                }
                let length = usize::from((u16::from(hvcc[p]) << 8) | u16::from(hvcc[p + 1]));
                p += 2;
                if length < 2 || length > hvcc.len() - p {
                    return Err(HeifError::invalid("'hvcC' 的 NAL 单元被截断。"));
                }
                if nal_units.len() >= MaxNalUnits {
                    return Err(HeifError::invalid("'hvcC' 的 NAL 单元太多。"));
                }
                nal_units.push(hvcc[p..p + length].to_vec());
                p += length;
            }
        }
        Ok(HevcDecoderConfiguration { length_size, nal_units })
    }

    /// 记录的 SPS（第一个），解析后用于 head-only 读取。
    pub fn parse_sps(&self) -> HeifResult<HevcSps> {
        for nal in &self.nal_units {
            if Nal::nal_type(nal)? == Nal::SpsNut {
                return HevcSps::parse(&Nal::unescape_to_array(nal));
            }
        }
        Err(HeifError::invalid("'hvcC' box 没有 SPS。"))
    }
}
