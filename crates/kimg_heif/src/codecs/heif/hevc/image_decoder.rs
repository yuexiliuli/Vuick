//! HEVC 图像 item 解码器：`hvcC` 参数集 + item 的长度前缀 NAL 单元。
//! **本阶段编译绿的 stub**：`decode_item` 给出签名，函数体返回 `NotImplemented`（后续阶段接入
//! `HevcFrameDecoder` 全链）。NAL 汇总/校验逻辑（含 MaxNalUnits=4096）照 C# `Hevc\HevcImageDecoder.cs`
//! 留待实现，此处仅保签名与常量。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::hvcc::HevcDecoderConfiguration;
use crate::codecs::heif::hevc::picture::HevcPicture;

/// 一个图像 item 内 NAL 单元的硬上限。
pub const MaxNalUnits: usize = 4096;

/// 解码一个 HEVC 编码的图像 item（参数集来自 `hvcC`，加 item 的长度前缀 NAL 单元）。
///
/// `config`：`hvcC` 解码器配置（长度前缀位宽 + 参数集）。
/// `payload`：item 的原始载荷（`iloc` 解析结果）。
pub fn decode_item(
    _config: &HevcDecoderConfiguration,
    _payload: &[u8],
) -> HeifResult<HevcPicture> {
    Err(HeifError::not_implemented(
        "HEVC 图像 item 解码（CABAC/帧内/变换）尚未实现。",
    ))
}
