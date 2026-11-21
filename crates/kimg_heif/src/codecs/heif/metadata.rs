//! HEIF 文件给主图像附加的元数据 item（23008-12 §6.4.9 / Annex A）：`Exif` 与 XMP `mime` item，
//! 每个都用 `cdsc`（"content describes"）引用链到主 item；`thmb` 缩略图是同样的图像 item。
//! 通过 `iloc` 解析 item 数据、受文件边界限制；畸形 item 读作缺省。
//! 移植自 C# `HeifMetadata.cs`。

use crate::codecs::heif::container::HeifContainer;
use crate::codecs::heif::isobmff::item_container::{Item, ItemContainer};

const RefCdsc: u32 = 0x6364_7363; // 'cdsc'
const RefThmb: u32 = 0x7468_6D62; // 'thmb'
const ItemExif: u32 = 0x4578_6966; // 'Exif'
const ItemMime: u32 = 0x6D69_6D65; // 'mime'
const ItemHvc1: u32 = 0x6876_6331; // 'hvc1'
const MaxItemBytes: i64 = 16 << 20;

/// 主 item 的 Exif / XMP 块与其 ICC profile。
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
}

impl Metadata {
    /// 读取描述为主 item 元数据的 Exif 和 XMP 块，及其 ICC profile。
    pub fn read(data: &[u8], container: &HeifContainer<'_>) -> Metadata {
        let mut exif: Option<Vec<u8>> = None;
        let mut xmp: Option<Vec<u8>> = None;
        let items = &container.items;
        let primary_id = items.primary_item_id;
        for reference in &items.references {
            if reference.ty != RefCdsc || !reference.to.contains(&primary_id) {
                continue;
            }
            let Some(item) = items.find(reference.from) else { continue };
            if item.ty == ItemExif && exif.is_none() {
                exif = read_exif(data, items, item);
            } else if item.ty == ItemMime && xmp.is_none() && Metadata::is_xmp(&item.content_type) {
                xmp = read_item(data, items, item);
            }
        }
        Metadata {
            exif,
            xmp,
            icc: container.icc_profile.clone(),
        }
    }

    /// 主图像的第一个 `hvc1` 缩略图 item（若有）；返回其在 items 里的下标。
    pub fn find_thumbnail(container: &HeifContainer<'_>) -> Option<usize> {
        let items = &container.items;
        let primary_id = items.primary_item_id;
        for reference in &items.references {
            if reference.ty != RefThmb || !reference.to.contains(&primary_id) {
                continue;
            }
            if let Some(idx) = items.find_index(reference.from)
                && items.items[idx].ty == ItemHvc1 {
                    return Some(idx);
                }
        }
        None
    }

    fn is_xmp(content_type: &Option<String>) -> bool {
        matches!(
            content_type.as_deref(),
            Some("application/rdf+xml" | "application/xmp+xml" | "text/xml")
        )
    }
}

fn read_exif(data: &[u8], items: &ItemContainer<'_>, item: &Item) -> Option<Vec<u8>> {
    let payload = read_item(data, items, item)?;
    if payload.len() < 4 {
        return None;
    }
    let offset = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
    if offset > payload.len() - 4 {
        return None;
    }
    let mut tiff = &payload[4 + offset..];
    // 有些写入方在头前放 "Exif\0\0" 并把它算进 offset，有些不。
    if tiff.starts_with(b"Exif\0\0") {
        tiff = &tiff[6..];
    }
    if tiff.len() >= 8 {
        Some(tiff.to_vec())
    } else {
        None
    }
}

fn read_item(data: &[u8], items: &ItemContainer<'_>, item: &Item) -> Option<Vec<u8>> {
    let length = ItemContainer::get_item_length(item);
    if length <= 0 || length > data.len() as i64 || length > MaxItemBytes {
        return None;
    }
    let mut buffer = vec![0u8; length as usize];
    if items.resolve(item, &mut buffer).is_err() {
        return None;
    }
    Some(buffer)
}
