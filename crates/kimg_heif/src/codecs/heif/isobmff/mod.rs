//! ISOBMFF（ISO/IEC 14496-12）box 层：`BoxReader` + `ItemContainer`（与 AVIF 共享的 item 模型）。
//! 移植自 C# `Sdcb.SimdCodecs.Shared\Isobmff\{BoxReader,ItemContainer}.cs`（去掉条件编译）。

pub mod box_reader;
pub mod item_container;

pub use box_reader::BoxReader;
pub use item_container::{Extent, Item, ItemContainer, Property, Reference};
