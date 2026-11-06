//! HEVC（H.265）解码子模块：NAL 工具 + RBSP 位读取 + 参数集（VPS/SPS/PPS）+ hvcC + 切片头。
//! 解码算法模块（cabac/intra_pred/transform/loop_filter/sao/frame_decoder/image_decoder）为
//! 编译绿的 stub，后续阶段逐模块落地（对准 C# `Sdcb.SimdCodecs.Heif\Hevc\`）。

pub mod bit_reader;
pub mod cabac;
pub mod frame_decoder;
pub mod hvcc;
pub mod image_decoder;
pub mod intra_pred;
pub mod loop_filter;
pub mod nal;
pub mod parameter_sets;
pub mod picture;
pub mod sao;
pub mod slice_header;
pub mod transform;
// 逆变换 SIMD 内核叶子（`#![allow(unsafe_code)]`；transform.rs 分派到本模块）。hevc 内部可见。
pub(super) mod transform_simd;

pub use bit_reader::HevcBitReader;
pub use nal::Nal;
pub use hvcc::HevcDecoderConfiguration;
pub use image_decoder::{decode_item, decode_item_flags};
pub use parameter_sets::{HevcPps, HevcScan, HevcScalingList, HevcSps};
pub use picture::HevcPicture;
pub use slice_header::HevcSliceHeader;
