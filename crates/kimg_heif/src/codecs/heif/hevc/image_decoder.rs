//! HEVC 图像 item 解码器：`hvcC` 参数集 + item 的长度前缀 NAL 单元。
//! 逐行移植自 C# `Hevc\HevcImageDecoder.cs`：NAL 汇总/校验（含 MaxNalUnits=4096）、
//! SPS/PPS 解析、带内 SPS 不得改图画尺寸的复核，然后 `HevcFrameDecoder::decode` 串行解码。
//! C# 侧并行的 wavefront 路径（`CanDecodeWavefront`/`TryDecodeWavefront`）是纯性能优化，
//! 串行路径字节恒等，这里不移植（Phase-3 滤波接入前无关紧要）。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::frame_decoder::HevcFrameDecoder;
use crate::codecs::heif::hevc::hvcc::HevcDecoderConfiguration;
use crate::codecs::heif::hevc::nal::Nal;
use crate::codecs::heif::hevc::parameter_sets::{HevcPps, HevcSps};
use crate::codecs::heif::hevc::picture::HevcPicture;

/// 一个图像 item 内 NAL 单元的硬上限。
pub const MaxNalUnits: usize = 4096;

/// 解码一个 HEVC 编码的图像 item（参数集来自 `hvcC`，加 item 的长度前缀 NAL 单元）。
///
/// `config`：`hvcC` 解码器配置（长度前缀位宽 + 参数集）。
/// `payload`：item 的原始载荷（`iloc` 解析结果）。
pub fn decode_item(
    config: &HevcDecoderConfiguration,
    payload: &[u8],
) -> HeifResult<HevcPicture> {
    decode_item_flags(config, payload, false, false)
}

/// 与 [`decode_item`] 相同，但可消融 in-loop 滤波（oracle `_nofilter` 对照）。
/// `skip_deblock`/`skip_sao` 为真时跳过对应环节，输出 = C# `-deblock -sao` 消融。
pub fn decode_item_flags(
    config: &HevcDecoderConfiguration,
    payload: &[u8],
    skip_deblock: bool,
    skip_sao: bool,
) -> HeifResult<HevcPicture> {
    let mut sps: Option<HevcSps> = None;
    let mut pps: Option<HevcPps> = None;
    let mut slices: Vec<(u8, Vec<u8>)> = Vec::new();

    // C# 的 Accept 局部函数：接收一个 NAL 单元并分类汇总。
    fn accept(
        nal: &[u8],
        sps: &mut Option<HevcSps>,
        pps: &mut Option<HevcPps>,
        slices: &mut Vec<(u8, Vec<u8>)>,
    ) -> HeifResult<()> {
        if nal.len() < 2 {
            return Err(HeifError::invalid("NAL 单元比它的头还短。"));
        }
        if (nal[0] & 0x80) != 0 {
            return Err(HeifError::invalid("forbidden_zero_bit 已设置。"));
        }
        let ty = Nal::nal_type(nal)?;
        let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        if layer_id != 0 {
            return Ok(()); // 其他层（如多层扩展）忽略
        }
        match ty {
            Nal::SpsNut => {
                *sps = Some(HevcSps::parse(&Nal::unescape_to_array(nal))?);
            }
            Nal::PpsNut => {
                *pps = Some(HevcPps::parse(&Nal::unescape_to_array(nal))?);
            }
            // VCL NAL 单元：TRAIL/TSA/STSA/RADL/RASL（0..9）与 IRAP（16..21）。
            t if t <= 21 && (t <= 9 || t >= 16) => {
                slices.push((t, Nal::unescape_to_array(nal)));
            }
            _ => {}
        }
        Ok(())
    }

    for nal in &config.nal_units {
        accept(nal, &mut sps, &mut pps, &mut slices)?;
    }

    let length_size = config.length_size;
    let mut p = 0usize;
    while p < payload.len() {
        if payload.len() - p < length_size {
            return Err(HeifError::invalid("NAL 单元长度字段被截断。"));
        }
        let mut length: i64 = 0;
        for i in 0..length_size {
            length = (length << 8) | i64::from(payload[p + i]);
        }
        p += length_size;
        if length > (payload.len() - p) as i64 {
            return Err(HeifError::invalid("NAL 单元越出 item 末尾。"));
        }
        if slices.len() >= MaxNalUnits {
            return Err(HeifError::invalid("图像 item 内 NAL 单元太多。"));
        }
        if length > 0 {
            accept(
                &payload[p..p + length as usize],
                &mut sps,
                &mut pps,
                &mut slices,
            )?;
        }
        p += length as usize;
    }

    let sps = sps.ok_or_else(|| HeifError::invalid("图像 item 没有 SPS。"))?;
    let pps = pps.ok_or_else(|| HeifError::invalid("图像 item 没有 PPS。"))?;
    if pps.sps_id != sps.id {
        return Err(HeifError::invalid("PPS 引用了不在的图像 SPS。"));
    }
    if slices.is_empty() {
        return Err(HeifError::invalid("图像 item 没有编码切片。"));
    }
    // 容器已按 hvcC 的 SPS 检查了几何（并限定了分配）；带内 SPS 不得改图画尺寸。
    let declared = config.parse_sps()?;
    if sps.width != declared.width
        || sps.height != declared.height
        || sps.conf_left != declared.conf_left
        || sps.conf_right != declared.conf_right
        || sps.conf_top != declared.conf_top
        || sps.conf_bottom != declared.conf_bottom
    {
        return Err(HeifError::invalid("带内 SPS 改变 hvcC 声明的图画尺寸。"));
    }

    let mut decoder = HevcFrameDecoder::new(sps, pps, false, false)?;
    decoder.set_skip_filters(skip_deblock, skip_sao);
    decoder.decode(slices)
}
