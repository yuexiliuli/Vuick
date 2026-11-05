//! 画布合成（C# `HeifDecoder.DecodeYuv` 一比一移植）：把各 grid tile 的 Y/Cb/Cr 平面按行列贴到
//! 画布并裁剪到 grid 输出尺寸，得到 transform 前整图 YUV。供 oracle 测试（字节恒等对 C#）与
//! 后续 `decode_rgba` 集成用。clap/irot/imir 属 transform 阶段，本模块不处理（外部分流）。
//! 色度按 4:2:0（`shift=1`）子抽样放位；conformance 窗口已在 brightness 采样格里。

use crate::codecs::heif::error::{HeifError, HeifResult};

/// 一个 grid tile 的解码 YUV 平面（对应 `HevcPicture` 相关字段；tile 尺寸即 SPS 尺寸）。
#[derive(Debug, Clone)]
pub struct TileYuv {
    /// Y 平面（行主序，stride = `y_w`）。
    pub y: Vec<u16>,
    /// Cb 平面（行主序，stride = `c_w`）。
    pub cb: Vec<u16>,
    /// Cr 平面（行主序，stride = `c_w`）。
    pub cr: Vec<u16>,
    /// SPS 亮度平面宽（Y 平面行距）。
    pub y_w: i32,
    /// SPS 亮度平面高。
    pub y_h: i32,
    /// 色度平面宽（行距）。
    pub c_w: i32,
    /// 色度平面高。
    pub c_h: i32,
    /// conformance 窗口（亮度采样）：左/右/上/下。
    pub crop_left: i32,
    pub crop_right: i32,
    pub crop_top: i32,
    pub crop_bottom: i32,
}

impl TileYuv {
    /// 裁剪后输出亮度宽（C# `OutputWidth`）。
    pub fn output_width(&self) -> i32 {
        self.y_w - self.crop_left - self.crop_right
    }
    /// 裁剪后输出亮度高（C# `OutputHeight`）。
    pub fn output_height(&self) -> i32 {
        self.y_h - self.crop_top - self.crop_bottom
    }
}

/// 合成后的整图 YUV（transform 前，画布尺度）。
#[derive(Debug, Clone)]
pub struct ComposedYuv {
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
    pub width: i32,
    pub height: i32,
}

impl ComposedYuv {
    pub fn chroma_width(&self) -> i32 {
        (self.width + 1) >> 1
    }
    pub fn chroma_height(&self) -> i32 {
        (self.height + 1) >> 1
    }
}

/// 把 grid tiles（行主序，第 i 个在 (i % cols, i / cols)）合成到 `canvas_w × canvas_h` 画布的
/// Y/Cb/Cr 平面；色度平面 `(cw, ch) = ((w+1)/2, (h+1)/2)`，tile 裁剪到画布边界（对应 C#
/// `HeifDecoder.DecodeYuv` 的逐平面拷贝）。几何/边界非法返回 `Err`（不 panic）。
pub fn compose_grid(
    tiles: &[TileYuv],
    cols: i32,
    rows: i32,
    canvas_w: i32,
    canvas_h: i32,
) -> HeifResult<ComposedYuv> {
    if cols <= 0 || rows <= 0 || canvas_w <= 0 || canvas_h <= 0 {
        return Err(HeifError::invalid("grid/画布尺寸非法。"));
    }
    if tiles.is_empty() {
        return Err(HeifError::invalid("grid 没有 tile。"));
    }
    if (tiles.len() as i64) > i64::from(cols) * i64::from(rows) {
        return Err(HeifError::invalid("grid tile 数超过 列×行。"));
    }
    let cw = (canvas_w + 1) >> 1;
    let ch = (canvas_h + 1) >> 1;
    let y_cap = (canvas_w as usize) * (canvas_h as usize);
    let c_cap = (cw as usize) * (ch as usize);
    let mut y = vec![0u16; y_cap];
    let mut cb = vec![0u16; c_cap];
    let mut cr = vec![0u16; c_cap];

    for (i, t) in tiles.iter().enumerate() {
        let col = (i as i32) % cols;
        let row = (i as i32) / cols;
        let tw = t.output_width();
        let th = t.output_height();
        if tw <= 0 || th <= 0 {
            return Err(HeifError::invalid("tile 的 conformance 窗口为空。"));
        }
        // 逐平面：0=Y、1=Cb、2=Cr；色度采样坐标 ÷2（4:2:0）。
        for c in 0..3usize {
            let shift = if c == 0 { 0 } else { 1 };
            let dst_w = if c == 0 { canvas_w } else { cw };
            let dst_h = if c == 0 { canvas_h } else { ch };
            let dst_x = (col * tw) >> shift;
            let dst_y = (row * th) >> shift;
            let src_stride = if c == 0 { t.y_w } else { t.c_w };
            let src_h = if c == 0 { t.y_h } else { t.c_h };
            let src_x = t.crop_left >> shift;
            let src_y = t.crop_top >> shift;
            let copy_w = i32::min(tw >> shift, dst_w - dst_x);
            let copy_h = i32::min(th >> shift, dst_h - dst_y);
            // tile 落在画布外或与画布无重合：C# `DecodeYuv` 的 `Math.Min` 对此会给非正拷贝
            // （for 循环零次），dump 结果等同跳过；负拷贝在 C# 抛异常但可辩护处理=跳过，
            // 与整图 YUV oracle（transform 前画布）逐字节一致。
            if copy_w <= 0 || copy_h <= 0 {
                continue;
            }
            if dst_x < 0 || dst_y < 0 || dst_x >= dst_w || dst_y >= dst_h || src_x < 0 || src_y < 0 {
                return Err(HeifError::invalid("tile 位置/尺寸越界。"));
            }
            let (src_plane, dst_plane): (&[u16], &mut [u16]) = match c {
                0 => (&t.y, &mut y),
                1 => (&t.cb, &mut cb),
                _ => (&t.cr, &mut cr),
            };
            paste_plane(
                src_plane,
                src_stride,
                src_h,
                src_x,
                src_y,
                copy_w,
                copy_h,
                dst_plane,
                dst_w,
                dst_x,
                dst_y,
            )?;
        }
    }
    Ok(ComposedYuv {
        y,
        cb,
        cr,
        width: canvas_w,
        height: canvas_h,
    })
}

/// 把源平面 `[src_y..src_y+copy_h) × [src_x..src_x+copy_w)` 拷贝进目标平面
/// `[dst_y..dst_y+copy_h) × [dst_x..dst_x+copy_w)`（两平面都行主序、无跨步、stride = 平面宽）。
fn paste_plane(
    src: &[u16],
    src_stride: i32,
    src_h: i32,
    src_x: i32,
    src_y: i32,
    copy_w: i32,
    copy_h: i32,
    dst: &mut [u16],
    dst_stride: i32,
    dst_x: i32,
    dst_y: i32,
) -> HeifResult<()> {
    let src_end = (src_stride * src_h) as usize;
    let dst_end = dst.len();
    let sw = copy_w as usize;
    for yy in 0..copy_h {
        let so = ((src_y + yy) as usize) * (src_stride as usize) + (src_x as usize);
        let do_ = ((dst_y + yy) as usize) * (dst_stride as usize) + (dst_x as usize);
        if so + sw > src_end || so + sw > src.len() || do_ + sw > dst_end {
            return Err(HeifError::invalid("tile 平面拷贝越界。"));
        }
        dst[do_..do_ + sw].copy_from_slice(&src[so..so + sw]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(w: i32, h: i32, fill: u16) -> TileYuv {
        TileYuv {
            y: vec![fill; (w * h) as usize],
            cb: vec![fill; (w * w) as usize / 4],
            cr: vec![fill; (w * w) as usize / 4],
            y_w: w,
            y_h: h,
            c_w: w / 2,
            c_h: h / 2,
            crop_left: 0,
            crop_right: 0,
            crop_top: 0,
            crop_bottom: 0,
        }
    }

    #[test]
    fn compose_two_x_two_tiles() {
        // 2x2 tiles of 2x2(亮度) 拼成 canvas 4x4；每 tile 用不同填充值验证放位。
        let t0 = tile(2, 2, 1);
        let t1 = tile(2, 2, 2);
        let t2 = tile(2, 2, 3);
        let t3 = tile(2, 2, 4);
        let out = compose_grid(&[t0, t1, t2, t3], 2, 2, 4, 4).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
        // Y: 左上 = 1, 右上 = 2, 左下 = 3, 右下 = 4。
        assert_eq!(out.y[0], 1);
        assert_eq!(out.y[2], 2);
        assert_eq!(out.y[8], 3);
        assert_eq!(out.y[10], 4);
        assert!(!out.y.iter().any(|&v| v == 0));
    }

    #[test]
    fn compose_crops_edge_tile() {
        // 1x2 grid，canvas 3x2：tile1 在 (1,0) 只露出 1 列宽。
        let t0 = tile(2, 2, 9);
        let t1 = tile(2, 2, 7);
        let out = compose_grid(&[t0, t1], 2, 1, 3, 2).unwrap();
        assert_eq!(out.y.len(), 6);
        assert_eq!(out.y[0], 9); // col0=t0
        assert_eq!(out.y[1], 9);
        assert_eq!(out.y[2], 7); // col2=t1 第一列
        assert_eq!(out.y[3], 9);
        assert_eq!(out.y[4], 9);
        assert_eq!(out.y[5], 7);
        // 色度：canvas 3x2 → cw=2, ch=1；两个 tile 各占 c0/c1。
        assert_eq!(out.cb.len(), 2);
        assert_eq!(out.cb[0], 9);
        assert_eq!(out.cb[1], 7);
    }

    #[test]
    fn compose_applies_crop_window() {
        // 单 tile 带 conformance crop_bottom=1，canvas 高 1。
        let mut t = tile(2, 2, 5);
        t.crop_bottom = 1;
        let out = compose_grid(&[t], 1, 1, 2, 1).unwrap();
        assert_eq!(out.y.len(), 2);
        assert_eq!(out.y[0], 5);
        assert_eq!(out.y[1], 5);
    }

    #[test]
    fn compose_rejects_bad_geometry() {
        assert!(compose_grid(&[tile(2, 2, 0)], 0, 1, 2, 2).is_err());
        assert!(compose_grid(&[], 1, 1, 2, 2).is_err());
        // tile 数超过 cols×rows。
        assert!(compose_grid(&[tile(2, 2, 0), tile(2, 2, 0)], 1, 1, 4, 2).is_err());
    }
}
