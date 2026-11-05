//! kimg_heif：kimg 拆分的 HEIF/HEIC 解码 crate。
//!
//! 镜像 C# `Sdcb.SimdCodecs.Heif`（纯托管 HEVC 解码器）的仓库实际使用子集，逐字节 oracle 对齐。
//! 本阶段（Phase-1 骨架）交付：ISOBMFF 容器层 + HEIF 参数集（VPS/SPS/PPS + hvcC）+ NAL/位读取 +
//! 切片头 + 帧解码器门面（`HeifDecoder` 等价物）；HEVC 解码算法（CABAC/intra/transform/lf/sao/
//! picture）只建编译绿的 stub，后续阶段补齐。
//! 运行时零第三方依赖（仅 std + kimg_core 共享类型）。

// 常量/关联常量镜像 C# PascalCase 命名（BoxFtyp/ItemHvc1/HevcCabacTables.SaoMerge…），
// 便于逐字段对拍；按 C# 命名保留，关掉大小写 lint。
#![allow(non_upper_case_globals)]

pub mod codecs;

pub use kimg_core::{ColorType, ImageDecoder};

pub use codecs::heif::{decode_rgba, decode_yuv, read_info, HeicDecoder, HeifError, HeifInfo};
pub use codecs::heif::probe::{heif_probe, ProbeInfo};
