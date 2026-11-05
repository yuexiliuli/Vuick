//! transform 属性（clap/irot/imir）应用（C# `HeifCanvas.cs` 构造器一比一移植）。
//!
//! C# 把按关联顺序的 transforms 合成为一张**仿射图** output = (a·x + b·y + c, d·x + e·y + f)
//! （a,b,d,e ∈ {−1,0,1}：带符号置换 + 平移；23008-12 §6.5.1 的像素地图），画布像素（x,y）经
//! 它落到显示朝向输出。合成图是当前矩形 → 下一矩形的双射，故画布像素命中最终
//! `[0,width)×[0,height)` 输出矩形当且仅当它在 preimage 矩形里——逐画布像素扫描 +
//! 越界守卫即与 C#「逐瓦片写输出」逐字节一致（零 panic）。
//!
//! 本模块只做纯整数仿射；解码 → grid 合成 → 色转属于 canvas/color，各自处理。

use crate::codecs::heif::container::{HeifContainer, Transform, PropClap, PropIrot};
use crate::codecs::heif::error::{HeifError, HeifResult};

/// 合成的画布 → 输出仿射图与输出尺寸（C# `HeifCanvas` 的 `_a.._f` 与 `Width/Height`）。
#[derive(Debug, Clone, Copy)]
pub struct TransformMap {
    pub a: i32,
    pub b: i32,
    pub c: i32,
    pub d: i32,
    pub e: i32,
    pub f: i32,
    pub width: i32,
    pub height: i32,
}

/// 按文件关联顺序合成变换（C# `HeifCanvas` 构造器的 transforms 循环）：`canvas_w × canvas_h`
/// 画布 → `(a,b,c,d,e,f)` 映射 + 输出 `width × height`（= 显示尺寸，与 `DisplaySize()` 一致）。
/// `clap` 矩形非法（like C# 抛 `InvalidDataException`）时返回 `Err`。
pub fn compose(canvas_w: i32, canvas_h: i32, transforms: &[Transform]) -> HeifResult<TransformMap> {
    let mut a = 1i32;
    let mut b = 0i32;
    let mut c = 0i32;
    let mut d = 0i32;
    let mut e = 1i32;
    let mut f = 0i32;
    let mut w = canvas_w;
    let mut h = canvas_h;

    for t in transforms {
        match t.ty {
            PropClap => {
                // C#：crop 当前矩形 [x0, x0+w)×[y0, y0+h) → output' = output − (x0, y0)。
                let (x0, y0, cw, ch) = HeifContainer::clap_rectangle(
                    t.clap.as_ref().ok_or_else(|| HeifError::invalid("'clap' 缺少参数。"))?,
                    w,
                    h,
                )?;
                c -= x0;
                f -= y0;
                w = cw;
                h = ch;
            }
            PropIrot => match t.a {
                // 逆时针四分之一圈：1 → (y, W − 1 − x)，2 → (W − 1 − x, H − 1 − y)，3 → (H − 1 − y, x)。
                // 右值全部读**旧** a..f 与旧 w/h（C# 元组赋值同义）。
                1 => {
                    let (na, nb, nc, nd, ne, nf) = (d, e, f, -a, -b, w - 1 - c);
                    (a, b, c, d, e, f) = (na, nb, nc, nd, ne, nf);
                    std::mem::swap(&mut w, &mut h);
                }
                2 => {
                    let (na, nb, nc, nd, ne, nf) = (-a, -b, w - 1 - c, -d, -e, h - 1 - f);
                    (a, b, c, d, e, f) = (na, nb, nc, nd, ne, nf);
                }
                3 => {
                    let (na, nb, nc, nd, ne, nf) = (-d, -e, h - 1 - f, a, b, c);
                    (a, b, c, d, e, f) = (na, nb, nc, nd, ne, nf);
                    std::mem::swap(&mut w, &mut h);
                }
                _ => {}
            },
            _ => {
                // imir：axis 0 镜像左↔右（改 x 侧），axis 1 镜像上↔下（改 y 侧）。
                if t.a == 0 {
                    let (na, nb, nc) = (-a, -b, w - 1 - c);
                    (a, b, c) = (na, nb, nc);
                } else {
                    let (nd, ne, nf) = (-d, -e, h - 1 - f);
                    (d, e, f) = (nd, ne, nf);
                }
            }
        }
    }
    Ok(TransformMap {
        a,
        b,
        c,
        d,
        e,
        f,
        width: w,
        height: h,
    })
}

/// 把画布 RGBA8（`canvas_w × canvas_h` 行主序无跨步）经 `map` 写到 `out`（显示朝向 `width × height`
/// 行主序、`out` 至少 `width*height*4` 字节）。画布像素映射出输出的被跳过（preimage 即输出恰覆盖
/// 的矩形，见模块头注）。C# 逐 tile 写输出与逐画布扫描等价，逐字节同。
pub fn apply_rgba(canvas: &[u8], canvas_w: i32, canvas_h: i32, map: &TransformMap, out: &mut [u8]) -> HeifResult<()> {
    let ow = map.width;
    let oh = map.height;
    if canvas_w <= 0 || canvas_h <= 0 || ow <= 0 || oh <= 0 {
        return Err(HeifError::invalid("transform 画布/输出尺寸非法。"));
    }
    let canvas_px = (canvas_w as usize)
        .checked_mul(canvas_h as usize)
        .ok_or_else(|| HeifError::invalid("画布尺寸溢出。"))?;
    if canvas.len() < canvas_px * 4 {
        return Err(HeifError::invalid("画布 RGBA 缓冲不足。"));
    }
    let out_px = (ow as usize)
        .checked_mul(oh as usize)
        .ok_or_else(|| HeifError::invalid("输出尺寸溢出。"))?;
    if out.len() < out_px * 4 {
        return Err(HeifError::invalid("输出 RGBA 缓冲不足。"));
    }
    let a = map.a;
    let b = map.b;
    let c = map.c;
    let d = map.d;
    let e = map.e;
    let f = map.f;
    for y in 0..canvas_h {
        let src_row = (y as usize) * (canvas_w as usize);
        for x in 0..canvas_w {
            let ox = a * x + b * y + c;
            let oy = d * x + e * y + f;
            if ox < 0 || ox >= ow || oy < 0 || oy >= oh {
                continue;
            }
            let si = (src_row + x as usize) * 4;
            let di = ((oy as usize) * (ow as usize) + ox as usize) * 4;
            out[di..di + 4].copy_from_slice(&canvas[si..si + 4]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::heif::container::{PropImir};

    fn id_map(w: i32, h: i32) -> TransformMap {
        TransformMap { a: 1, b: 0, c: 0, d: 0, e: 1, f: 0, width: w, height: h }
    }

    #[test]
    fn compose_irot_1() {
        // 2x1 画布 + 90°：1 → (y, W−1−x)。W=2,H=1 → 输出 1x2。
        let m = compose(2, 1, &[Transform { ty: PropIrot, a: 1, b: 0, clap: None }]).unwrap();
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (0, 1, 0, -1, 0, 1));
        assert_eq!((m.width, m.height), (1, 2));
    }

    #[test]
    fn compose_irot_3() {
        // 2x1 + 270°：3 → (H−1−y, x)。W=2,H=1 → 输出 1x2。
        let m = compose(2, 1, &[Transform { ty: PropIrot, a: 3, b: 0, clap: None }]).unwrap();
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (0, -1, 0, 1, 0, 0));
        assert_eq!((m.width, m.height), (1, 2));
    }

    #[test]
    fn compose_imir_axis0() {
        // 2x1 + 镜像左↔右：x → W−1−x = 1−x。
        let m = compose(2, 1, &[Transform { ty: PropImir, a: 0, b: 0, clap: None }]).unwrap();
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (-1, 0, 1, 0, 1, 0));
        assert_eq!((m.width, m.height), (2, 1));
    }

    #[test]
    fn compose_clap_then_irot() {
        // 画布 4x3，clap (x0=1,y0=1,w=2,h=1) → 输出 2x1；随后 irot 1 → 1x2，映射带子：
        // clap 后 map=identity 平移 (−1,−1)；irot 1 再用当前 W=2：→ (d,e,f,−a,−b,W−1−c)。
        let clap = [2i64, 1, 1, 1, 0, 1, 0, 1]; // w=2,h=1,xoff=0,yoff=0 → left=(0)+(4-2)/2=1, top=(4-...)=1
        let m = compose(
            4,
            3,
            &[
                Transform { ty: PropClap, a: 0, b: 0, clap: Some(clap) },
                Transform { ty: PropIrot, a: 1, b: 0, clap: None },
            ],
        )
        .unwrap();
        // clap: c=−1, f=−1, w=2,h=1。irot1: (a,b,c,d,e,f)=(d,e,f,−a,−b,w−1−c)=(0,1,−1,−1,0,2−1−(−1)=2)
        assert_eq!((m.a, m.b, m.c, m.d, m.e, m.f), (0, 1, -1, -1, 0, 2));
        assert_eq!((m.width, m.height), (1, 2));
    }

    #[test]
    fn apply_irot1_small() {
        // 画布 2x1：pixel (0,0)=[1,2,3,255], (1,0)=[4,5,6,255]。90° → 输出 1x2：
        // (x,y)=(0,0)→(0,1)； (1,0)→(0,0)。
        let m = compose(2, 1, &[Transform { ty: PropIrot, a: 1, b: 0, clap: None }]).unwrap();
        let canvas = [1, 2, 3, 255, 4, 5, 6, 255];
        let mut out = vec![0u8; 8];
        apply_rgba(&canvas, 2, 1, &m, &mut out).unwrap();
        assert_eq!(out[0..4], [4, 5, 6, 255]);
        assert_eq!(out[4..8], [1, 2, 3, 255]);
    }

    #[test]
    fn apply_imir_axis0_small() {
        // 画布 2x1 + 镜像：输出仍是 2x1，pixel 交换。
        let m = compose(2, 1, &[Transform { ty: PropImir, a: 0, b: 0, clap: None }]).unwrap();
        let canvas = [1, 2, 3, 255, 4, 5, 6, 255];
        let mut out = vec![0u8; 8];
        apply_rgba(&canvas, 2, 1, &m, &mut out).unwrap();
        assert_eq!(out[0..4], [4, 5, 6, 255]);
        assert_eq!(out[4..8], [1, 2, 3, 255]);
    }

    #[test]
    fn apply_identity_is_copy() {
        let m = id_map(2, 2);
        let canvas = [9; 16];
        let mut out = vec![0u8; 16];
        apply_rgba(&canvas, 2, 2, &m, &mut out).unwrap();
        assert_eq!(out, canvas);
    }

    #[test]
    fn apply_tiny_buffer_rejected() {
        // out 缓冲不足 → Err（不 panic）。
        let m = id_map(2, 2);
        let mut out = vec![0u8; 4];
        assert!(apply_rgba(&[0u8; 16], 2, 2, &m, &mut out).is_err());
    }
}
