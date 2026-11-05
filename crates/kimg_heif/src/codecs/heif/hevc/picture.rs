//! 解码出的 HEVC 图画：三块采样平面（全部位深按 16 位存），外加定义输出图画的 conformance 裁剪窗
//! （§7.4.3.2.1）。移植自 C# `Hevc\HevcPicture.cs`。

/// 一张解码出的 HEVC 图画。
pub struct HevcPicture {
    pub width: i32,
    pub height: i32,
    pub chroma_format_idc: i32,
    pub bit_depth_y: i32,
    pub bit_depth_c: i32,
    pub sub_width_c: i32,
    pub sub_height_c: i32,
    pub chroma_width: i32,
    pub chroma_height: i32,

    /// Y、Cb、Cr，行主序，stride = 平面宽。
    pub planes: [Vec<u16>; 3],

    /// 亮度采样里的 conformance window。
    pub crop_left: i32,
    pub crop_right: i32,
    pub crop_top: i32,
    pub crop_bottom: i32,
}

impl HevcPicture {
    pub fn new(
        width: i32,
        height: i32,
        chroma_format_idc: i32,
        bit_depth_y: i32,
        bit_depth_c: i32,
    ) -> HevcPicture {
        let sub_width_c = if chroma_format_idc == 1 || chroma_format_idc == 2 { 2 } else { 1 };
        let sub_height_c = if chroma_format_idc == 1 { 2 } else { 1 };
        let chroma_width = if chroma_format_idc == 0 { 0 } else { width / sub_width_c };
        let chroma_height = if chroma_format_idc == 0 { 0 } else { height / sub_height_c };
        HevcPicture {
            width,
            height,
            chroma_format_idc,
            bit_depth_y,
            bit_depth_c,
            sub_width_c,
            sub_height_c,
            chroma_width,
            chroma_height,
            planes: [
                vec![0u16; (width as usize) * (height as usize)],
                vec![0u16; (chroma_width as usize) * (chroma_height as usize)],
                vec![0u16; (chroma_width as usize) * (chroma_height as usize)],
            ],
            crop_left: 0,
            crop_right: 0,
            crop_top: 0,
            crop_bottom: 0,
        }
    }

    pub fn output_width(&self) -> i32 {
        self.width - self.crop_left - self.crop_right
    }
    pub fn output_height(&self) -> i32 {
        self.height - self.crop_top - self.crop_bottom
    }

    pub fn plane_width(&self, c_idx: usize) -> i32 {
        if c_idx == 0 { self.width } else { self.chroma_width }
    }
    pub fn plane_height(&self, c_idx: usize) -> i32 {
        if c_idx == 0 { self.height } else { self.chroma_height }
    }
}
