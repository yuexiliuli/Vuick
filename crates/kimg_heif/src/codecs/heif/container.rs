//! HEIF（ISO/IEC 23008-12）对一个 ISOBMFF item 容器的解读：主图像 item、`grid` 派生图像及其
//! tiles，以及解码器要的属性（`hvcC`、`ispe`、`colr`、`clap`、`irot`、`imir`）。
//! 移植自 C# `HeifContainer.cs`（字段/分支一比一）。

use std::rc::Rc;

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::isobmff::item_container::ItemContainer;
use crate::codecs::heif::hevc::hvcc::HevcDecoderConfiguration;

pub const ItemHvc1: u32 = 0x6876_6331; // 'hvc1'
pub const ItemGrid: u32 = 0x6772_6964; // 'grid'
pub const RefDimg: u32 = 0x6469_6D67; // 'dimg'
pub const PropHvcC: u32 = 0x6876_6343; // 'hvcC'
pub const PropIspe: u32 = 0x6973_7065; // 'ispe'
pub const PropColr: u32 = 0x636F_6C72; // 'colr'
pub const PropClap: u32 = 0x636C_6170; // 'clap'
pub const PropIrot: u32 = 0x6972_6F74; // 'irot'
pub const PropImir: u32 = 0x696D_6972; // 'imir'
const ColrNclx: u32 = 0x6E63_6C78; // 'nclx'
const ColrProf: u32 = 0x7072_6F66; // 'prof'
const ColrRicc: u32 = 0x7249_4343; // 'rICC'

/// 接受的最大画布（像素；比任何手机摄像头输出都宽裕得多）。
pub const MaxPixels: i64 = 1i64 << 28;

/// 接受的最大画布单边。
pub const MaxDimension: i32 = 65_535;

/// 一个 transform 属性（clap/irot/imir）。
#[derive(Debug, Clone)]
pub struct Transform {
    pub ty: u32,
    pub a: i32,
    pub b: i32,
    pub clap: Option<[i64; 8]>,
}

/// `colr`/nclx 色彩描述。
#[derive(Debug, Clone, Copy)]
pub struct Nclx {
    pub colour_primaries: i32,
    pub transfer_characteristics: i32,
    pub matrix_coefficients: i32,
    pub full_range: bool,
}

/// HEIF item 容器的 HEIF 解读。
pub struct HeifContainer<'a> {
    pub items: ItemContainer<'a>,
    pub primary: usize,
    /// 编码图像 item（grid 顺序，行主）；非派生主图像是一个条目。
    pub tiles: Vec<usize>,
    pub grid_rows: i32,
    pub grid_columns: i32,
    /// transform 前的画布尺寸：grid 输出尺寸，或编码图像的尺寸。
    pub canvas_width: i32,
    pub canvas_height: i32,
    /// 每个不同 `hvcC` 的解码器配置（按 tile 下标）。
    pub tile_configs: Vec<Rc<HevcDecoderConfiguration>>,
    tile_config_offsets: Vec<i64>,
    pub icc_profile: Option<Vec<u8>>,
    pub nclx_colour: Option<Nclx>,
    /// 主 item 的 transform 属性（按关联顺序，23008-12 §6.5.1）。
    pub transforms: Vec<Transform>,
}

impl<'a> HeifContainer<'a> {
    pub fn parse(data: &'a [u8]) -> HeifResult<HeifContainer<'a>> {
        Self::parse_item(data, 0)
    }

    /// 与 [`Self::parse`] 相同，但用 item `item_id` 代替主 item（0 = 主 item）：`thmb` 缩略图、
    /// 独立图像 item 这样解码。
    pub fn parse_item(data: &'a [u8], item_id_arg: u32) -> HeifResult<HeifContainer<'a>> {
        let items = ItemContainer::parse(data)?;
        let mut item_id = item_id_arg;
        if item_id == 0 {
            item_id = items.primary_item_id;
        }
        if item_id == 0 {
            return Err(HeifError::invalid("HEIF 文件没有主 item ('pitm')。"));
        }
        let primary = items
            .find_index(item_id)
            .ok_or_else(|| HeifError::invalid("图像 item 没有在 'iinf' 里声明。"))?;
        let primary_type = items.find(item_id).map(|i| i.ty).unwrap_or(0);

        let mut container = HeifContainer {
            items,
            primary,
            tiles: Vec::new(),
            grid_rows: 1,
            grid_columns: 1,
            canvas_width: 0,
            canvas_height: 0,
            tile_configs: Vec::new(),
            tile_config_offsets: Vec::new(),
            icc_profile: None,
            nclx_colour: None,
            transforms: Vec::new(),
        };

        if primary_type == ItemHvc1 {
            container.tiles.push(container.primary);
        } else if primary_type == ItemGrid {
            container.parse_grid()?;
        } else {
            return Err(HeifError::invalid(format!(
                "HEIF 主 item 类型 '{}' 不受支持；只支持 'hvc1' 和 'grid'。",
                four_cc(primary_type)
            )));
        }

        let mut previous_offset: i64 = -1;
        let mut previous_config: Option<Rc<HevcDecoderConfiguration>> = None;
        let tile_count = container.tiles.len();
        for t in 0..tile_count {
            let tile_id = {
                let tile_ref = &container.items.items[container.tiles[t]];
                if tile_ref.ty != ItemHvc1 {
                    return Err(HeifError::invalid(format!(
                        "grid tile 类型 '{}' 不受支持。",
                        four_cc(tile_ref.ty)
                    )));
                }
                tile_ref.id
            };
            let (hvcc_off, hvcc_len) = container
                .items
                .find_property_offset(tile_id, PropHvcC)
                .ok_or_else(|| HeifError::invalid("'hvc1' item 没有 'hvcC' 属性。"))?;
            let off_i64 = hvcc_off as i64;
            let config = if off_i64 != previous_offset {
                let c = Rc::new(HevcDecoderConfiguration::parse(&data[hvcc_off..hvcc_off + hvcc_len])?);
                previous_offset = off_i64;
                previous_config = Some(c.clone());
                c
            } else {
                previous_config.clone().ok_or_else(|| {
                    HeifError::invalid("'hvcC' 配置缓存异常。")
                })?
            };
            container.tile_configs.push(config);
            container.tile_config_offsets.push(off_i64);
        }

        // 颜色：主 item 自己的 'colr'，否则第一个 tile 的（手机常把它放在 tiles 上）。
        let primary_id = container.items.items[container.primary].id;
        container.read_colour(primary_id);
        if container.icc_profile.is_none()
            && container.nclx_colour.is_none()
            && container.tiles[0] != container.primary
        {
            let first_tile_id = container.items.items[container.tiles[0]].id;
            container.read_colour(first_tile_id);
        }

        container.read_transforms(primary_id);

        if primary_type == ItemHvc1 {
            // 编码尺寸（conformance window 之后）是权威：取自 SPS。
            let sps = container.tile_configs[0].parse_sps()?;
            container.canvas_width = sps.width - sps.conf_left - sps.conf_right;
            container.canvas_height = sps.height - sps.conf_top - sps.conf_bottom;
            if i64::from(container.canvas_width) * i64::from(container.canvas_height) > MaxPixels {
                return Err(HeifError::invalid("图像太大。"));
            }
        }
        container.validate_geometry()?;
        Ok(container)
    }

    /// 显示尺寸（transform 属性之后）。`clap` 矩形非法时（C# 抛 `InvalidDataException`）返回 Err。
    pub fn display_size(&self) -> HeifResult<(i32, i32)> {
        let mut w = self.canvas_width;
        let mut h = self.canvas_height;
        for t in &self.transforms {
            if t.ty == PropClap {
                let r = Self::clap_rectangle(t.clap.as_ref().unwrap(), w, h)?;
                w = r.2;
                h = r.3;
            } else if t.ty == PropIrot && (t.a & 1) != 0 {
                std::mem::swap(&mut w, &mut h);
            }
        }
        Ok((w, h))
    }

    /// clean aperture（ISO/IEC 14496-12 §12.1.4）在 `width` × `height` 图上的裁剪矩形：光圈居中于
    /// ((width-1)/2 + horizOff, (height-1)/2 + vertOff)。分数边缘取最近的采样；矩形裁剪进图画。
    pub fn clap_rectangle(clap: &[i64; 8], width: i32, height: i32) -> HeifResult<(i32, i32, i32, i32)> {
        let (w_n, w_d) = (clap[0], clap[1]);
        let (h_n, h_d) = (clap[2], clap[3]);
        let (x_n, x_d) = (clap[4], clap[5]);
        let (y_n, y_d) = (clap[6], clap[7]);
        if w_d == 0 || h_d == 0 || x_d == 0 || y_d == 0 {
            return Err(HeifError::invalid("'clap' 的分母为零。"));
        }
        let clap_w = w_n as f64 / w_d as f64;
        let clap_h = h_n as f64 / h_d as f64;
        let left = x_n as f64 / x_d as f64 + (width as f64 - clap_w) / 2.0;
        let top = y_n as f64 / y_d as f64 + (height as f64 - clap_h) / 2.0;
        let x0 = left.round().clamp(0.0, width as f64) as i32;
        let y0 = top.round().clamp(0.0, height as f64) as i32;
        let x1 = (left + clap_w).round().clamp(x0 as f64, width as f64) as i32;
        let y1 = (top + clap_h).round().clamp(y0 as f64, height as f64) as i32;
        if x1 == x0 || y1 == y0 {
            return Err(HeifError::invalid("'clap' clean aperture 为空。"));
        }
        Ok((x0, y0, x1 - x0, y1 - y0))
    }

    fn parse_grid(&mut self) -> HeifResult<()> {
        // ImageGrid（23008-12 §6.6.2.3.2）：version, flags, rows_minus_one, columns_minus_one, 输出尺寸。
        let grid = self.items.items[self.primary].clone();
        let length = ItemContainer::get_item_length(&grid);
        if !(8..=64).contains(&length) {
            return Err(HeifError::invalid("'grid' item 载荷尺寸非法。"));
        }
        let mut payload = vec![0u8; length as usize];
        let _ = self.items.resolve(&grid, &mut payload)?;
        if payload[0] != 0 {
            return Err(HeifError::invalid(format!(
                "不支持的 'grid' 版本 {}。",
                payload[0]
            )));
        }
        let wide = (payload[1] & 1) != 0;
        self.grid_rows = i32::from(payload[2]) + 1;
        self.grid_columns = i32::from(payload[3]) + 1;
        if wide {
            if length < 12 {
                return Err(HeifError::invalid("'grid' item 载荷被截断。"));
            }
            let w = read_u32(&payload[4..]);
            let h = read_u32(&payload[8..]);
            if w > MaxDimension as u32 || h > MaxDimension as u32 {
                return Err(HeifError::invalid("grid 太大。"));
            }
            self.canvas_width = w as i32;
            self.canvas_height = h as i32;
        } else {
            self.canvas_width = (i32::from(payload[4]) << 8) | i32::from(payload[5]);
            self.canvas_height = (i32::from(payload[6]) << 8) | i32::from(payload[7]);
        }
        if self.canvas_width == 0 || self.canvas_height == 0 {
            return Err(HeifError::invalid("grid 尺寸为零。"));
        }
        if i64::from(self.canvas_width) * i64::from(self.canvas_height) > MaxPixels {
            return Err(HeifError::invalid("grid 太大。"));
        }

        let dimg = self
            .items
            .references
            .iter()
            .find(|r| r.ty == RefDimg && r.from == grid.id)
            .cloned();
        let dimg = dimg.ok_or_else(|| HeifError::invalid("'grid' item 没有 'dimg' 引用。"))?;
        if dimg.to.len() != (self.grid_rows * self.grid_columns) as usize {
            return Err(HeifError::invalid(format!(
                "grid 是 {}x{}，但引用了 {} 个 tile。",
                self.grid_rows,
                self.grid_columns,
                dimg.to.len()
            )));
        }
        for id in &dimg.to {
            let idx = self
                .items
                .find_index(*id)
                .ok_or_else(|| HeifError::invalid(format!("grid tile item {id} 没有声明。")))?;
            self.tiles.push(idx);
        }
        Ok(())
    }

    /// 从 `hvcC` 参数集、任何解码/分配之前，检查编码图像尺寸一致、覆盖画布、总尺寸不要求小文件
    /// 做无界分配（每个 tile 的 SPS 可声明高达 16 888² 采样）。
    fn validate_geometry(&self) -> HeifResult<()> {
        let mut coded_samples: i64 = 0;
        let mut tile_w = -1i32;
        let mut tile_h = -1i32;
        let mut parsed: Option<(Rc<HevcDecoderConfiguration>, crate::codecs::heif::hevc::parameter_sets::HevcSps)> = None;
        for config in &self.tile_configs {
            let sps = match &parsed {
                Some((c, s)) if Rc::ptr_eq(c, config) => s.clone(),
                _ => {
                    let s = config.parse_sps()?;
                    parsed = Some((config.clone(), s.clone()));
                    s
                }
            };
            let w = sps.width - sps.conf_left - sps.conf_right;
            let h = sps.height - sps.conf_top - sps.conf_bottom;
            if tile_w < 0 {
                tile_w = w;
                tile_h = h;
            } else if w != tile_w || h != tile_h {
                return Err(HeifError::invalid("grid tiles 尺寸不同。"));
            }
            coded_samples += i64::from(sps.width) * i64::from(sps.height);
        }

        if coded_samples > 2 * MaxPixels {
            return Err(HeifError::invalid("编码图像太大。"));
        }
        if i64::from(tile_w) * i64::from(self.grid_columns) < i64::from(self.canvas_width)
            || i64::from(tile_h) * i64::from(self.grid_rows) < i64::from(self.canvas_height)
        {
            return Err(HeifError::invalid("grid tiles 不覆盖输出图像。"));
        }
        Ok(())
    }

    fn read_colour(&mut self, item_id: u32) {
        let Some(indices) = self.items.associations.get(&item_id) else {
            return;
        };
        for &index in indices {
            if index == 0 || index as usize > self.items.properties.len() {
                continue;
            }
            let property = self.items.properties[index as usize - 1];
            if property.ty != PropColr || property.length < 4 {
                continue;
            }
            let body = &self.items.data_ref()[property.offset..property.offset + property.length];
            let colour_type = read_u32(body);
            if colour_type == ColrNclx && body.len() >= 11 && self.nclx_colour.is_none() {
                self.nclx_colour = Some(Nclx {
                    colour_primaries: (i32::from(body[4]) << 8) | i32::from(body[5]),
                    transfer_characteristics: (i32::from(body[6]) << 8) | i32::from(body[7]),
                    matrix_coefficients: (i32::from(body[8]) << 8) | i32::from(body[9]),
                    full_range: (body[10] & 0x80) != 0,
                });
            } else if (colour_type == ColrProf || colour_type == ColrRicc) && self.icc_profile.is_none() {
                self.icc_profile = Some(body[4..].to_vec());
            }
        }
    }

    fn read_transforms(&mut self, item_id: u32) {
        let Some(indices) = self.items.associations.get(&item_id) else {
            return;
        };
        for &index in indices {
            if index == 0 || index as usize > self.items.properties.len() {
                continue;
            }
            let property = self.items.properties[index as usize - 1];
            let body = &self.items.data_ref()[property.offset..property.offset + property.length];
            match property.ty {
                PropIrot if body.len() >= 1 => {
                    self.transforms.push(Transform {
                        ty: PropIrot,
                        a: i32::from(body[0] & 3),
                        b: 0,
                        clap: None,
                    });
                }
                PropImir if body.len() >= 1 => {
                    self.transforms.push(Transform {
                        ty: PropImir,
                        a: i32::from(body[0] & 1),
                        b: 0,
                        clap: None,
                    });
                }
                PropClap if body.len() >= 32 => {
                    let mut clap = [0i64; 8];
                    for i in 0..8 {
                        let raw = read_u32(&body[i * 4..]);
                        // horizOffN (4) 与 vertOffN (6) 有符号；其余无符号。
                        clap[i] = if i == 4 || i == 6 {
                            i64::from(raw as i32)
                        } else {
                            i64::from(raw)
                        };
                    }
                    self.transforms.push(Transform {
                        ty: PropClap,
                        a: 0,
                        b: 0,
                        clap: Some(clap),
                    });
                }
                _ => {}
            }
        }
    }
}

fn read_u32(s: &[u8]) -> u32 {
    (u32::from(s[0]) << 24) | (u32::from(s[1]) << 16) | (u32::from(s[2]) << 8) | u32::from(s[3])
}

fn four_cc(ty: u32) -> String {
    let mut s = String::with_capacity(4);
    s.push((ty >> 24) as u8 as char);
    s.push(((ty >> 16) & 0xFF) as u8 as char);
    s.push(((ty >> 8) & 0xFF) as u8 as char);
    s.push((ty & 0xFF) as u8 as char);
    s
}
