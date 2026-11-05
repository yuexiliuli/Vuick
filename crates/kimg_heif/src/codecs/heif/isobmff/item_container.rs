//! 通用 ISOBMFF meta-box item 模型（ISO/IEC 14496-12 §8.11）：`pitm`、`iinf`、`iloc`、
//! `iprp`、`iref`、`idat`。HEIF（23008-12）与 AVIF 都把静态图放进同一个 item 模型，故本层不
//! 感知编解码器：属性以原始切片交付，各包自己解析认识的那部分。
//! 移植自 C# `Sdcb.SimdCodecs.Shared\Isobmff\ItemContainer.cs`（去掉 `#if SIMDCODECS_HEIF` 条件编译）。
//!
//! 不可信输入纪律：每个偏移使用前都对照文件长度校验，item/extent/property/子 box 数量封顶，嵌套深度
//! 封顶——畸形文件返回 `HeifError::InvalidData` 而非死循环/无限递归/无界分配。容器不拷贝 item 载荷，
//! `resolve_extent` 直接切片。

use std::collections::HashMap;

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::isobmff::box_reader::{
    BoxReader, BOX_FTYP, BOX_HDLR, BOX_IDAT, BOX_IINF, BOX_ILOC, BOX_INFE, BOX_IPCO, BOX_IPMA,
    BOX_IPRP, BOX_IREF, BOX_META, BOX_PICT, BOX_PITM,
};

/// 一个容器内 item 数的硬上限（`iinf` 计数是 16 位字段）。
const MaxItems: usize = 65_535;
/// 单个 item 的 extent 数硬上限。
const MaxExtents: usize = 65_535;
/// 单个 `ipco` 内共享属性数硬上限。
const MaxProperties: usize = 4_096;
/// box 嵌套深度硬上限（防构造文件耗尽栈）。
const MaxDepth: usize = 16;
/// `infe` item_type 'mime'。
const ItemMime: u32 = 0x6D69_6D65;

/// 一个数据 extent（offset/length 为攻击者可控制的 32/64 位字段，一律 long 运算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: i64,
    pub length: i64,
}

/// 一个 item（`iinf` 声明，`iloc` 附 extent）。
#[derive(Debug, Clone)]
pub struct Item {
    pub id: u32,
    pub ty: u32,
    pub construction_method: u32,
    /// `mime` item 的 content_type（如 XMP 的 `application/rdf+xml`），否则 None。
    pub content_type: Option<String>,
    /// item 数据位置（按序；零长度 extent 是纯 offset 条目）。
    pub extents: Vec<Extent>,
}

/// 一条共享属性（`ipco` 顺序；`ipma` 关联是 1 基的）。
#[derive(Debug, Clone, Copy)]
pub struct Property {
    pub ty: u32,
    pub offset: usize,
    pub length: usize,
}

/// 一条 `iref` 引用（文件顺序）。
#[derive(Debug, Clone)]
pub struct Reference {
    pub ty: u32,
    pub from: u32,
    pub to: Vec<u32>,
}

/// 已解析的 ISOBMFF item 容器。持有输入字节的借用：调用方在整个使用期必须保持同一缓冲。
pub struct ItemContainer<'a> {
    data: &'a [u8],
    items_by_id: HashMap<u32, usize>,
    /// 全部 item（`iinf` 顺序）。
    pub items: Vec<Item>,
    /// `pitm` 主 item ID，无则 0。
    pub primary_item_id: u32,
    /// 共享属性（`ipco` 顺序）。
    pub properties: Vec<Property>,
    /// item → 关联属性下标（1 基，与 `ipma` 写入一致）。
    pub associations: HashMap<u32, Vec<u32>>,
    /// `iref` 引用（文件顺序）。
    pub references: Vec<Reference>,
    /// `idat` payload 偏移，无则 -1。
    pub idat_offset: i64,
    pub idat_length: i64,
    /// `ftyp` 的 major brand，无则 0。
    pub major_brand: u32,
}

impl<'a> ItemContainer<'a> {
    /// 借出底层字节缓冲（供 `read_colour`/`read_transforms` 切片属性）。
    pub(crate) fn data_ref(&self) -> &'a [u8] {
        self.data
    }

    /// 解析容器元数据。返回对象只引用 `data` 的偏移，调用方须在整个 `ItemContainer` 存活期保持缓冲。
    pub fn parse(data: &'a [u8]) -> HeifResult<ItemContainer<'a>> {
        if data.len() < 8 {
            return Err(HeifError::invalid("文件太短，不是 ISOBMFF 容器。"));
        }
        let mut container = ItemContainer {
            data,
            items_by_id: HashMap::new(),
            items: Vec::new(),
            primary_item_id: 0,
            properties: Vec::new(),
            associations: HashMap::new(),
            references: Vec::new(),
            idat_offset: -1,
            idat_length: 0,
            major_brand: 0,
        };
        let mut meta_seen = false;
        let mut reader = BoxReader::new_full(data);
        while reader.has_box() {
            let position = reader.position();
            let header = reader
                .try_read_box()
                .ok_or_else(|| HeifError::invalid(format!("偏移 {position} 处的 box 头畸形。")))?;
            match header.ty {
                BOX_FTYP => {
                    if header.box_end - header.payload >= 8 {
                        container.major_brand = reader.read_u32be(header.payload);
                    }
                }
                BOX_META => {
                    container.parse_meta(header.payload, header.box_end)?;
                    meta_seen = true;
                }
                _ => {}
            }
        }
        if !meta_seen {
            return Err(HeifError::invalid("容器没有 'meta' box；不是 HEIF/AVIF 静态图。"));
        }
        Ok(container)
    }

    /// 指定 id 的 item（无则 None）。
    pub fn find(&self, id: u32) -> Option<&Item> {
        self.items_by_id.get(&id).map(|&i| &self.items[i])
    }

    /// 指定 id 的 item 在 `items` 里的下标（无则 None）。
    pub fn find_index(&self, id: u32) -> Option<usize> {
        self.items_by_id.get(&id).copied()
    }

    /// 把 item 的数据（全部 extent 拼接）拷入 `destination`（须至少 `get_item_length` 字节），
    /// 返回写入字节数。
    pub fn resolve(&self, item: &Item, destination: &mut [u8]) -> HeifResult<usize> {
        let mut written = 0usize;
        for &extent in &item.extents {
            let source = self.resolve_extent(item, extent)?;
            if written + source.len() > destination.len() {
                return Err(HeifError::invalid("item 数据放不进提供的缓冲。"));
            }
            destination[written..written + source.len()].copy_from_slice(source);
            written += source.len();
        }
        Ok(written)
    }

    /// item 全部 extent 的总字节数。
    pub fn get_item_length(item: &Item) -> i64 {
        // wrapping：与 C# unchecked 相加回绕一致（extent 长度是攻击者可控制的字段）
        item.extents.iter().fold(0i64, |a, e| a.wrapping_add(e.length))
    }

    /// 把单个 extent 解析为 `data` 的切片，应用构造方法：
    /// 0 = 文件绝对偏移；1 = `idat` 内偏移。方法 2（item 偏移）静态图不用，直接拒绝。
    pub fn resolve_extent(&self, item: &Item, extent: Extent) -> HeifResult<&'a [u8]> {
        let base_offset: i64 = match item.construction_method {
            0 => 0,
            1 if self.idat_offset >= 0 => self.idat_offset,
            1 => {
                return Err(HeifError::invalid(
                    "item 构造方法 1 需要 'idat' box。",
                ));
            }
            m => {
                return Err(HeifError::invalid(format!(
                    "不支持的 item 构造方法 {m}。"
                )));
            }
        };

        // 长整型算术里校验：offset/length 是攻击者可控制的 32/64 位字段（C# unchecked 回绕 → wrapping_add）。
        let start = base_offset.wrapping_add(extent.offset);
        let length = extent.length;
        if start < 0 || length < 0 || start > self.data.len() as i64 || length > self.data.len() as i64 - start {
            return Err(HeifError::invalid("item extent 在文件之外。"));
        }

        let limit = if item.construction_method == 1 { self.idat_length } else { self.data.len() as i64 };
        if start + length > base_offset + limit {
            return Err(HeifError::invalid("item extent 在其基准 box 之外。"));
        }

        Ok(&self.data[start as usize..(start + length) as usize])
    }

    /// 与 `item_id` 关联的、类型 `ty` 的第一个属性 payload。
    pub fn find_property(&self, item_id: u32, ty: u32) -> Option<&'a [u8]> {
        let indices = self.associations.get(&item_id)?;
        for &index in indices {
            if index == 0 || index as usize > self.properties.len() {
                continue;
            }
            let property = &self.properties[index as usize - 1];
            if property.ty == ty {
                return Some(&self.data[property.offset..property.offset + property.length]);
            }
        }
        None
    }

    /// 与 `item_id` 关联的、类型 `ty` 的全部属性（按关联顺序）。
    pub fn find_properties(&self, item_id: u32, ty: u32) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        if let Some(indices) = self.associations.get(&item_id) {
            for &index in indices {
                if index == 0 || index as usize > self.properties.len() {
                    continue;
                }
                let property = &self.properties[index as usize - 1];
                if property.ty == ty {
                    out.push(&self.data[property.offset..property.offset + property.length]);
                }
            }
        }
        out
    }

    /// `item_id` 关联属性中类型 `ty` 的第一个 `(偏移, 长度)`。
    pub fn find_property_offset(&self, item_id: u32, ty: u32) -> Option<(usize, usize)> {
        let indices = self.associations.get(&item_id)?;
        for &index in indices {
            if index == 0 || index as usize > self.properties.len() {
                continue;
            }
            let property = &self.properties[index as usize - 1];
            if property.ty == ty {
                return Some((property.offset, property.length));
            }
        }
        None
    }

    fn parse_meta(&mut self, payload: usize, box_end: usize) -> HeifResult<()> {
        if payload + 4 > box_end {
            return Err(HeifError::invalid("'meta' box 被截断。"));
        }
        // 两趟遍历子 box：'iloc' 命名 'iinf' 声明的 item，文件可能先放哪个都行，所以 item 表在
        // 任何 'iloc' extent 挂上去之前已由 'iinf' 建好。
        let mut iloc: Option<(usize, usize)> = None;
        let mut iprp: Option<(usize, usize)> = None;
        let mut iref: Option<(usize, usize)> = None;

        // 'meta' 是 FullBox（version + flags），随后是子 box。
        let mut reader = BoxReader::new(self.data, payload + 4, box_end);
        while reader.has_box() {
            let position = reader.position();
            let header = reader.try_read_box().ok_or_else(|| {
                HeifError::invalid(format!("'meta' 内偏移 {position} 处的 box 头畸形。"))
            })?;
            match header.ty {
                BOX_HDLR => {
                    // handler 必须是 'pict'；其它 handler 说明不是静态图。
                    if header.box_end - header.payload < 12 {
                        return Err(HeifError::invalid("'hdlr' box 被截断。"));
                    }
                    if reader.read_u32be(header.payload + 8) != BOX_PICT {
                        return Err(HeifError::invalid(
                            "'meta' 的 handler 不是 'pict'；这不是静态图。",
                        ));
                    }
                }
                BOX_PITM => {
                    if header.box_end - header.payload < 6 {
                        return Err(HeifError::invalid("'pitm' box 被截断。"));
                    }
                    self.primary_item_id = if reader.read_u8(header.payload) == 0 {
                        u32::from(reader.read_u16be(header.payload + 4))
                    } else {
                        reader.read_u32be(header.payload + 4)
                    };
                }
                BOX_IINF => {
                    self.parse_iinf(header.payload, header.box_end)?;
                }
                BOX_IDAT => {
                    self.idat_offset = header.payload as i64;
                    self.idat_length = (header.box_end - header.payload) as i64;
                }
                BOX_ILOC => iloc = Some((header.payload, header.box_end)),
                BOX_IPRP => iprp = Some((header.payload, header.box_end)),
                BOX_IREF => iref = Some((header.payload, header.box_end)),
                _ => {}
            }
        }

        let (iloc_payload, iloc_end) =
            iloc.ok_or_else(|| HeifError::invalid("'meta' box 没有 'iloc'。"))?;
        self.parse_iloc(iloc_payload, iloc_end)?;
        if let Some((p, e)) = iprp {
            self.parse_iprp(p, e, 1)?;
        }
        if let Some((p, e)) = iref {
            self.parse_iref(p, e)?;
        }
        Ok(())
    }

    fn parse_iinf(&mut self, payload: usize, box_end: usize) -> HeifResult<()> {
        if payload + 4 > box_end {
            return Err(HeifError::invalid("'iinf' box 被截断。"));
        }
        let version = self.data[payload];
        // version 0 是 16 位条目计数，1 及之后是 32 位。
        let count_size = if version == 0 { 2 } else { 4 };
        if payload + 4 + count_size > box_end {
            return Err(HeifError::invalid("'iinf' 条目计数被截断。"));
        }

        let mut reader = BoxReader::new(self.data, payload + 4 + count_size, box_end);
        while reader.has_box() {
            let position = reader.position();
            let header = reader.try_read_box().ok_or_else(|| {
                HeifError::invalid(format!("'iinf' 内偏移 {position} 处应为 'infe' box。"))
            })?;
            if header.ty != BOX_INFE {
                return Err(HeifError::invalid(format!(
                    "'iinf' 内偏移 {position} 处应为 'infe' box。"
                )));
            }
            if self.items.len() >= MaxItems {
                return Err(HeifError::invalid("item 太多。"));
            }

            let entry_payload = header.payload;
            let entry_end = header.box_end;
            let infe = BoxReader::new(self.data, entry_payload, entry_end);
            if entry_end - entry_payload < 8 {
                return Err(HeifError::invalid("'infe' box 被截断。"));
            }
            let infe_version = infe.read_u8(entry_payload);
            // infe v2：16 位 item_ID + 16 位保护下标；v3：32 位 item_ID。v0/v1 不带 item type。
            let id_size = if infe_version >= 3 { 4 } else { 2 };
            if entry_payload + 4 + id_size + 2 > entry_end {
                return Err(HeifError::invalid("'infe' box 被截断。"));
            }
            let id = if id_size == 4 {
                infe.read_u32be(entry_payload + 4)
            } else {
                u32::from(infe.read_u16be(entry_payload + 4))
            };
            let mut cursor = entry_payload + 4 + id_size + 2;
            let mut ty = 0u32;
            if infe_version >= 2 {
                if cursor + 4 > entry_end {
                    return Err(HeifError::invalid("'infe' item type 被截断。"));
                }
                ty = infe.read_u32be(cursor);
                cursor += 4;
            }

            // item_name 是 NUL 结尾的 UTF-8 串；长度不需要，只跳过。
            if cursor > entry_end {
                return Err(HeifError::invalid("'infe' box 被截断。"));
            }
            let (_, new_cursor) = skip_c_string(self.data, cursor, entry_end)?;
            cursor = new_cursor;

            // 'mime' item 随后 用同样的 NUL 结尾方式给 content_type（14496-12 §8.11.6.2）。
            // 不写的写入方让 item 保持未打字，而不是让文件不可读。
            let mut content_type = None;
            if ty == ItemMime && cursor < entry_end {
                let start = cursor;
                let end = self.data[start..entry_end].iter().position(|&b| b == 0);
                if let Some(end) = end {
                    if end > 0 {
                        let take = end.min(255);
                        content_type = Some(String::from_utf8_lossy(&self.data[start..start + take]).into_owned());
                    }
                }
            }

            if id != 0 && !self.items_by_id.contains_key(&id) {
                self.items_by_id.insert(id, self.items.len());
                self.items.push(Item {
                    id,
                    ty,
                    construction_method: 0,
                    content_type,
                    extents: Vec::new(),
                });
            }
        }
        Ok(())
    }

    fn parse_iloc(&mut self, payload: usize, box_end: usize) -> HeifResult<()> {
        if payload + 4 > box_end {
            return Err(HeifError::invalid("'iloc' box 被截断。"));
        }
        let version = self.data[payload];
        if version > 2 {
            return Err(HeifError::invalid(format!("不支持的 'iloc' 版本 {version}。")));
        }
        let reader = BoxReader::new(self.data, payload + 4, box_end);

        let mut cursor = payload + 4;
        if cursor + 2 > box_end {
            return Err(HeifError::invalid("'iloc' box 被截断。"));
        }
        let offset_size = self.data[cursor] >> 4;
        let length_size = self.data[cursor] & 0x0F;
        // 第二字节：base_offset_size（高半字节）+ index_size（v1/v2）或保留（v0）。
        let base_offset_size = self.data[cursor + 1] >> 4;
        let index_size = if version == 1 || version == 2 {
            self.data[cursor + 1] & 0x0F
        } else {
            0
        };
        cursor += 2;

        if !matches!(offset_size, 0 | 4 | 8)
            || !matches!(length_size, 0 | 4 | 8)
            || !matches!(index_size, 0 | 4 | 8)
            || !matches!(base_offset_size, 0 | 4 | 8)
        {
            return Err(HeifError::invalid("'iloc' 字段宽度非法。"));
        }

        let item_count: usize;
        if version < 2 {
            if cursor + 2 > box_end {
                return Err(HeifError::invalid("'iloc' item 计数被截断。"));
            }
            item_count = usize::from(reader.read_u16be(cursor));
            cursor += 2;
        } else {
            if cursor + 4 > box_end {
                return Err(HeifError::invalid("'iloc' item 计数被截断。"));
            }
            item_count = reader.read_u32be(cursor) as usize;
            cursor += 4;
        }
        if item_count > MaxItems {
            return Err(HeifError::invalid("'iloc' item 太多。"));
        }

        for _ in 0..item_count {
            let item_id;
            if version < 2 {
                require(cursor, 2, box_end)?;
                item_id = u32::from(reader.read_u16be(cursor));
                cursor += 2;
            } else {
                require(cursor, 4, box_end)?;
                item_id = reader.read_u32be(cursor);
                cursor += 4;
            }

            let mut construction_method = 0u32;
            if version == 1 || version == 2 {
                require(cursor, 2, box_end)?;
                construction_method = u32::from(reader.read_u16be(cursor)) & 0x0F;
                cursor += 2;
            }

            require(cursor, 2, box_end)?;
            let data_reference_index = reader.read_u16be(cursor);
            cursor += 2;
            if data_reference_index != 0 {
                return Err(HeifError::invalid(
                    "item 数据存储在文件外部（'dref'）不受支持。",
                ));
            }

            let base_offset = read_variable(&reader, &mut cursor, base_offset_size as usize, box_end)?;
            require(cursor, 2, box_end)?;
            let extent_count = reader.read_u16be(cursor);
            cursor += 2;
            if extent_count as usize > MaxExtents {
                return Err(HeifError::invalid("单个 item 的 extent 太多。"));
            }

            let item_index = self.items_by_id.get(&item_id).copied();
            if let Some(idx) = item_index {
                self.items[idx].construction_method = construction_method;
            }

            for _ in 0..extent_count {
                if (version == 1 || version == 2) && index_size != 0 {
                    let _ = read_variable(&reader, &mut cursor, index_size as usize, box_end)?;
                }
                let extent_offset = read_variable(&reader, &mut cursor, offset_size as usize, box_end)?;
                let extent_length = read_variable(&reader, &mut cursor, length_size as usize, box_end)?;
                if let Some(idx) = item_index {
                    let offset = base_offset.wrapping_add(extent_offset);
                    if offset < 0 || extent_length < 0 {
                        return Err(HeifError::invalid("负的 item extent。"));
                    }
                    self.items[idx].extents.push(Extent {
                        offset,
                        length: extent_length,
                    });
                }
            }
        }
        Ok(())
    }

    fn parse_iprp(&mut self, start: usize, end: usize, depth: usize) -> HeifResult<()> {
        if depth > MaxDepth {
            return Err(HeifError::invalid("box 嵌套太深。"));
        }
        let mut reader = BoxReader::new(self.data, start, end);
        while reader.has_box() {
            let position = reader.position();
            let header = reader.try_read_box().ok_or_else(|| {
                HeifError::invalid(format!("'iprp' 内偏移 {position} 处的 box 头畸形。"))
            })?;
            match header.ty {
                BOX_IPCO => {
                    let mut ipco = BoxReader::new(self.data, header.payload, header.box_end);
                    while ipco.has_box() {
                        let property_start = ipco.position();
                        let prop = ipco.try_read_box().ok_or_else(|| {
                            HeifError::invalid(format!("偏移 {property_start} 处属性 box 畸形。"))
                        })?;
                        if self.properties.len() >= MaxProperties {
                            return Err(HeifError::invalid("属性太多。"));
                        }
                        self.properties.push(Property {
                            ty: prop.ty,
                            offset: prop.payload,
                            length: prop.box_end - prop.payload,
                        });
                    }
                }
                BOX_IPMA => {
                    self.parse_ipma(header.payload, header.box_end)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_ipma(&mut self, payload: usize, box_end: usize) -> HeifResult<()> {
        if payload + 8 > box_end {
            return Err(HeifError::invalid("'ipma' box 被截断。"));
        }
        let version = self.data[payload];
        let flags = (u32::from(self.data[payload + 1]) << 16)
            | (u32::from(self.data[payload + 2]) << 8)
            | u32::from(self.data[payload + 3]);
        let reader = BoxReader::new(self.data, payload, box_end);
        let mut cursor = payload + 4;
        let entry_count = reader.read_u32be(cursor) as usize;
        cursor += 4;
        if entry_count > MaxItems {
            return Err(HeifError::invalid("'ipma' 条目太多。"));
        }

        let wide_index = (flags & 1) != 0;
        for _ in 0..entry_count {
            if cursor + 2 > box_end {
                return Err(HeifError::invalid("'ipma' box 被截断。"));
            }
            let item_id;
            if version < 1 {
                item_id = u32::from(reader.read_u16be(cursor));
                cursor += 2;
            } else {
                if cursor + 4 > box_end {
                    return Err(HeifError::invalid("'ipma' box 被截断。"));
                }
                item_id = reader.read_u32be(cursor);
                cursor += 4;
            }

            if cursor + 1 > box_end {
                return Err(HeifError::invalid("'ipma' box 被截断。"));
            }
            let association_count = self.data[cursor];
            cursor += 1;
            let mut indices = Vec::with_capacity(association_count as usize);
            for _ in 0..association_count {
                let index;
                if wide_index {
                    if cursor + 2 > box_end {
                        return Err(HeifError::invalid("'ipma' box 被截断。"));
                    }
                    index = u32::from(reader.read_u16be(cursor)) & 0x7FFF;
                    cursor += 2;
                } else {
                    if cursor + 1 > box_end {
                        return Err(HeifError::invalid("'ipma' box 被截断。"));
                    }
                    index = u32::from(self.data[cursor] & 0x7F);
                    cursor += 1;
                }
                indices.push(index);
            }
            self.associations.insert(item_id, indices);
        }
        Ok(())
    }

    fn parse_iref(&mut self, payload: usize, box_end: usize) -> HeifResult<()> {
        if payload + 4 > box_end {
            return Err(HeifError::invalid("'iref' box 被截断。"));
        }
        let version = self.data[payload];
        let mut reader = BoxReader::new(self.data, payload + 4, box_end);
        while reader.has_box() {
            let position = reader.position();
            let header = reader.try_read_box().ok_or_else(|| {
                HeifError::invalid(format!("偏移 {position} 处的引用 box 畸形。"))
            })?;

            let id_size = if version == 0 { 2 } else { 4 };
            if header.box_end - header.payload < id_size + 2 {
                return Err(HeifError::invalid("'iref' 条目被截断。"));
            }
            let from = if id_size == 2 {
                u32::from(reader.read_u16be(header.payload))
            } else {
                reader.read_u32be(header.payload)
            };
            let count = reader.read_u16be(header.payload + id_size);
            let mut cursor = header.payload + id_size + 2;
            if count as usize > MaxItems || cursor as i64 + (count as i64) * (id_size as i64) > header.box_end as i64 {
                return Err(HeifError::invalid("'iref' 引用计数非法。"));
            }

            let mut to = Vec::with_capacity(count as usize);
            for _ in 0..count {
                to.push(if id_size == 2 {
                    u32::from(reader.read_u16be(cursor))
                } else {
                    reader.read_u32be(cursor)
                });
                cursor += id_size;
            }
            self.references.push(Reference { ty: header.ty, from, to });
        }
        Ok(())
    }
}

/// `cursor` 处至少 `count` 字节仍在 `box_end` 内；否则畸形。
/// 语义与 C# `at > boxEnd - count` 等价，另补 `cursor > boxEnd` 守卫防 usize 下溢。
fn require(cursor: usize, count: usize, box_end: usize) -> HeifResult<()> {
    if cursor > box_end || count > box_end - cursor {
        return Err(HeifError::invalid("'iloc' box 被截断。"));
    }
    Ok(())
}

/// 读一个 `size` 字节（0/4/8）大端变长字段并推进游标。
fn read_variable(
    reader: &BoxReader<'_>,
    cursor: &mut usize,
    size: usize,
    box_end: usize,
) -> HeifResult<i64> {
    if *cursor > box_end || size > box_end - *cursor {
        return Err(HeifError::invalid("'iloc' box 被截断。"));
    }
    let value: i64 = match size {
        0 => 0,
        4 => i64::from(reader.read_u32be(*cursor)),
        8 => reader.read_u64be(*cursor) as i64, // C# unchecked((long)…)：位保持
        _ => return Err(HeifError::invalid("'iloc' 字段宽度非法。")),
    };
    *cursor += size;
    Ok(value)
}

/// 跳过 NUL 结尾字符串，返回 `(总长, 新游标)`；未遇到 NUL 则畸形。
fn skip_c_string(data: &[u8], mut cursor: usize, end: usize) -> HeifResult<(usize, usize)> {
    let start = cursor;
    while cursor < end && data[cursor] != 0 {
        cursor += 1;
    }
    if cursor >= end {
        return Err(HeifError::invalid("NUL 结尾字符串被截断。"));
    }
    cursor += 1;
    Ok((cursor - start, cursor))
}
