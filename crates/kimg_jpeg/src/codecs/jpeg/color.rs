//! 颜色空间换算（1:1 复刻 zune-jpeg `color_convert/scalar.rs` 与 `worker.rs` 的颜色部分）。
//!
//! AVX2 颜色换算与标量字节恒等（同一整数系数与取整链），故统一采用标量实现。
//!
//! dormancy（K24）：C# 语义换主后，本模块仅被 `avx2.rs` 及其单测引用，不再被管线调用。

#![allow(dead_code)]

use core::cmp::min;

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::segment::ColorSpace;
use crate::codecs::jpeg::segment::MAX_COMPONENTS;

// Bt.601 全范围逆矩阵系数（14 位精度，保持 i16 内）。
pub(crate) const Y_CF: i16 = 16384;
pub(crate) const CR_CF: i16 = 22970;
pub(crate) const CB_CF: i16 = 29032;
pub(crate) const C_G_CR_COEF_1: i16 = -11700;
pub(crate) const C_G_CB_COEF_2: i16 = -5638;
pub(crate) const YUV_PREC: i16 = 14;
pub(crate) const YUV_RND: i16 = (1 << (YUV_PREC - 1)) - 1;

#[inline]
fn clamp(a: i32) -> u8 {
    a.clamp(0, 255) as u8
}

/// 快速 0..255 * 0..255 => 0..255 带舍入乘法（stb 借用）。
#[inline]
fn blinn_8x8(in_val: u8, y: u8) -> u8 {
    let t = i32::from(in_val) * i32::from(y) + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// YCbCr -> RGB/BGR（每 16 像素）。
pub(crate) fn ycbcr_to_rgb_inner_16_scalar<const BGRA: bool>(
    y: &[i16; 16],
    cb: &[i16; 16],
    cr: &[i16; 16],
    output: &mut [u8],
    pos: &mut usize,
) {
    let (_, output_position) = output.split_at_mut(*pos);

    let opt: &mut [u8; 48] = output_position
        .get_mut(0..48)
        .expect("Slice to small cannot write")
        .try_into()
        .unwrap();

    for ((&y, (cb, cr)), out) in y
        .iter()
        .zip(cb.iter().zip(cr.iter()))
        .zip(opt.chunks_exact_mut(3))
    {
        let cr = cr - 128;
        let cb = cb - 128;

        let y0 = i32::from(y) * i32::from(Y_CF) + i32::from(YUV_RND);

        let r = (y0 + i32::from(cr) * i32::from(CR_CF)) >> YUV_PREC;
        let g = (y0
            + i32::from(cr) * i32::from(C_G_CR_COEF_1)
            + i32::from(cb) * i32::from(C_G_CB_COEF_2))
            >> YUV_PREC;
        let b = (y0 + i32::from(cb) * i32::from(CB_CF)) >> YUV_PREC;

        if BGRA {
            out[0] = clamp(b);
            out[1] = clamp(g);
            out[2] = clamp(r);
        } else {
            out[0] = clamp(r);
            out[1] = clamp(g);
            out[2] = clamp(b);
        }
    }

    *pos += 48;
}

/// YCbCr -> RGBA/BGRA（每 16 像素）。
pub(crate) fn ycbcr_to_rgba_inner_16_scalar<const BGRA: bool>(
    y: &[i16; 16],
    cb: &[i16; 16],
    cr: &[i16; 16],
    output: &mut [u8],
    pos: &mut usize,
) {
    let (_, output_position) = output.split_at_mut(*pos);

    let opt: &mut [u8; 64] = output_position
        .get_mut(0..64)
        .expect("Slice to small cannot write")
        .try_into()
        .unwrap();
    for ((&y, (cb, cr)), out) in y
        .iter()
        .zip(cb.iter().zip(cr.iter()))
        .zip(opt.chunks_exact_mut(4))
    {
        let cr = cr - 128;
        let cb = cb - 128;

        let y0 = i32::from(y) * i32::from(Y_CF) + i32::from(YUV_RND);

        let r = (y0 + i32::from(cr) * i32::from(CR_CF)) >> YUV_PREC;
        let g = (y0
            + i32::from(cr) * i32::from(C_G_CR_COEF_1)
            + i32::from(cb) * i32::from(C_G_CB_COEF_2))
            >> YUV_PREC;
        let b = (y0 + i32::from(cb) * i32::from(CB_CF)) >> YUV_PREC;

        if BGRA {
            out[0] = clamp(b);
            out[1] = clamp(g);
            out[2] = clamp(r);
            out[3] = 255;
        } else {
            out[0] = clamp(r);
            out[1] = clamp(g);
            out[2] = clamp(b);
            out[3] = 255;
        }
    }
    *pos += 64;
}

/// YCbCr -> 灰度。
pub(crate) fn ycbcr_to_grayscale(y: &[i16], width: usize, padded_width: usize, output: &mut [u8]) {
    for (y_in, out) in y
        .chunks_exact(padded_width)
        .zip(output.chunks_exact_mut(width))
    {
        for (y, out) in y_in.iter().zip(out.iter_mut()) {
            *out = *y as u8;
        }
    }
}

/// Luma -> RGB（三通道复制）。
fn convert_luma_to_rgb(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
) {
    for (pix_w, y_w) in output
        .chunks_exact_mut(width * 3)
        .zip(mcu_block[0].chunks_exact(padded_width))
    {
        for (pix, c) in pix_w.chunks_exact_mut(3).zip(y_w) {
            pix[0] = *c as u8;
            pix[1] = *c as u8;
            pix[2] = *c as u8;
        }
    }
}

/// Luma -> RGBA（四通道复制，A=255）。
fn convert_luma_to_rgba(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
) {
    for (pix_w, y_w) in output
        .chunks_exact_mut(width * 4)
        .zip(mcu_block[0].chunks_exact(padded_width))
    {
        for (pix, c) in pix_w.chunks_exact_mut(4).zip(y_w) {
            pix[0] = *c as u8;
            pix[1] = *c as u8;
            pix[2] = *c as u8;
            pix[3] = 255;
        }
    }
}

/// 3 通道复制并去 padding。
fn copy_removing_padding(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
) {
    for (((pix_w, c_w), m_w), y_w) in output
        .chunks_exact_mut(width * 3)
        .zip(mcu_block[0].chunks_exact(padded_width))
        .zip(mcu_block[1].chunks_exact(padded_width))
        .zip(mcu_block[2].chunks_exact(padded_width))
    {
        for (((pix, c), y), m) in pix_w.chunks_exact_mut(3).zip(c_w).zip(m_w).zip(y_w) {
            pix[0] = *c as u8;
            pix[1] = *y as u8;
            pix[2] = *m as u8;
        }
    }
}

/// 4 通道复制并去 padding。
fn copy_removing_padding_4x(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
) {
    for ((((pix_w, c_w), m_w), y_w), k_w) in output
        .chunks_exact_mut(width * 4)
        .zip(mcu_block[0].chunks_exact(padded_width))
        .zip(mcu_block[1].chunks_exact(padded_width))
        .zip(mcu_block[2].chunks_exact(padded_width))
        .zip(mcu_block[3].chunks_exact(padded_width))
    {
        for ((((pix, c), y), m), k) in pix_w
            .chunks_exact_mut(4)
            .zip(c_w)
            .zip(m_w)
            .zip(y_w)
            .zip(k_w)
        {
            pix[0] = *c as u8;
            pix[1] = *y as u8;
            pix[2] = *m as u8;
            pix[3] = *k as u8;
        }
    }
}

/// 任意通道数复制去 padding（目前仅 2 通道）。
fn copy_removing_padding_generic(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
    channels: usize,
) {
    match channels {
        2 => {
            for ((pix_w, y_w), k_w) in output
                .chunks_exact_mut(width * channels)
                .zip(mcu_block[0].chunks_exact(padded_width))
                .zip(mcu_block[1].chunks_exact(padded_width))
            {
                for ((pix, c), k) in pix_w.chunks_exact_mut(2).zip(y_w).zip(k_w) {
                    pix[0] = *c as u8;
                    pix[1] = *k as u8;
                }
            }
        }
        _ => unreachable!(),
    }
}

/// YCCK -> RGB(A)。
fn color_convert_ycck_to_rgb<const NUM_COMPONENTS: usize>(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output_colorspace: ColorSpace,
    output: &mut [u8],
) {
    color_convert_ycbcr(
        mcu_block,
        width,
        padded_width,
        output_colorspace,
        output,
    );
    for (pix_w, m_w) in output
        .chunks_exact_mut(width * 3)
        .zip(mcu_block[3].chunks_exact(padded_width))
    {
        for (pix, m) in pix_w.chunks_exact_mut(NUM_COMPONENTS).zip(m_w) {
            let m = (*m) as u8;
            pix[0] = blinn_8x8(255 - pix[0], m);
            pix[1] = blinn_8x8(255 - pix[1], m);
            pix[2] = blinn_8x8(255 - pix[2], m);
        }
    }
}

/// CMYK -> RGB(A)。
fn color_convert_cymk_to_rgb<const NUM_COMPONENTS: usize>(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output: &mut [u8],
) {
    for ((((pix_w, c_w), m_w), y_w), k_w) in output
        .chunks_exact_mut(width * NUM_COMPONENTS)
        .zip(mcu_block[0].chunks_exact(padded_width))
        .zip(mcu_block[1].chunks_exact(padded_width))
        .zip(mcu_block[2].chunks_exact(padded_width))
        .zip(mcu_block[3].chunks_exact(padded_width))
    {
        for ((((pix, c), m), y), k) in pix_w
            .chunks_exact_mut(3)
            .zip(c_w)
            .zip(m_w)
            .zip(y_w)
            .zip(k_w)
        {
            let c = *c as u8;
            let m = *m as u8;
            let y = *y as u8;
            let k = *k as u8;

            pix[0] = blinn_8x8(c, k);
            pix[1] = blinn_8x8(m, k);
            pix[2] = blinn_8x8(y, k);
        }
    }
}

/// YCbCr -> RGB(A) 的整体换算（按 16 像素块处理，处理小宽度与尾部重叠）。
fn color_convert_ycbcr(
    mcu_block: &[&[i16]; MAX_COMPONENTS],
    width: usize,
    padded_width: usize,
    output_colorspace: ColorSpace,
    output: &mut [u8],
) {
    let num_components = output_colorspace.num_components();

    let stride = width * num_components;
    let mut temp = [0; 64];
    for (((y_width, cb_width), cr_width), out) in mcu_block[0]
        .chunks_exact(padded_width)
        .zip(mcu_block[1].chunks_exact(padded_width))
        .zip(mcu_block[2].chunks_exact(padded_width))
        .zip(output.chunks_exact_mut(stride))
    {
        if width < 16 {
            let mut y_out = [0; 16];
            let mut cb_out = [0; 16];
            let mut cr_out = [0; 16];
            y_out[0..min(y_width.len(), 16)]
                .copy_from_slice(&y_width[0..min(y_width.len(), 16)]);
            cb_out[0..min(cb_width.len(), 16)]
                .copy_from_slice(&cb_width[0..min(cb_width.len(), 16)]);
            cr_out[0..min(cr_width.len(), 16)]
                .copy_from_slice(&cr_width[0..min(cr_width.len(), 16)]);
            convert_16(
                &y_out,
                &cb_out,
                &cr_out,
                &mut temp,
                output_colorspace,
            );
            out[0..width * num_components].copy_from_slice(&temp[0..width * num_components]);
            continue;
        }

        for (((y, cb), cr), out_c) in y_width
            .chunks_exact(16)
            .zip(cb_width.chunks_exact(16))
            .zip(cr_width.chunks_exact(16))
            .zip(out.chunks_exact_mut(16 * num_components))
        {
            convert_16(
                y.try_into().unwrap(),
                cb.try_into().unwrap(),
                cr.try_into().unwrap(),
                out_c,
                output_colorspace,
            );
        }
        // 尾部不足 16 的像素：取最后 16 个重算并覆写
        for ((y, cb), cr) in y_width[width - 16..]
            .chunks_exact(16)
            .zip(cb_width[width - 16..].chunks_exact(16))
            .zip(cr_width[width - 16..].chunks_exact(16))
            .take(1)
        {
            convert_16(
                y.try_into().unwrap(),
                cb.try_into().unwrap(),
                cr.try_into().unwrap(),
                &mut temp,
                output_colorspace,
            );
        }

        let rem = out[(width - 16) * num_components..]
            .chunks_exact_mut(16 * num_components)
            .next()
            .unwrap();

        rem.copy_from_slice(&temp[0..rem.len()]);
    }
}

/// 16 像素 YCbCr 换算（RGB/RGBA 两分支）。
fn convert_16(
    y: &[i16; 16],
    cb: &[i16; 16],
    cr: &[i16; 16],
    out: &mut [u8],
    output_colorspace: ColorSpace,
) {
    match output_colorspace {
        ColorSpace::RGB => {
            crate::codecs::jpeg::avx2::ycbcr_to_rgb_16(y, cb, cr, out, &mut 0);
        }
        ColorSpace::RGBA => {
            crate::codecs::jpeg::avx2::ycbcr_to_rgba_16(y, cb, cr, out, &mut 0);
        }
        _ => unreachable!("只支持 RGB/RGBA 输出的 YCbCr 换算"),
    }
}

/// 顶层颜色换算入口（对应 zune `worker::color_convert`）。
pub(crate) fn color_convert(
    unprocessed: &[&[i16]; MAX_COMPONENTS],
    input_colorspace: ColorSpace,
    output_colorspace: ColorSpace,
    output: &mut [u8],
    width: usize,
    padded_width: usize,
) -> Result<(), DecodeError> {
    if input_colorspace.num_components() == 3 && input_colorspace == output_colorspace {
        copy_removing_padding(unprocessed, width, padded_width, output);
        return Ok(());
    }
    if input_colorspace.num_components() == 4 && input_colorspace == output_colorspace {
        copy_removing_padding_4x(unprocessed, width, padded_width, output);
        return Ok(());
    }
    match (input_colorspace, output_colorspace) {
        (ColorSpace::YCbCr | ColorSpace::Luma, ColorSpace::Luma) => {
            ycbcr_to_grayscale(unprocessed[0], width, padded_width, output);
        }
        (ColorSpace::YCbCr, ColorSpace::RGB | ColorSpace::RGBA) => {
            color_convert_ycbcr(unprocessed, width, padded_width, output_colorspace, output);
        }
        (ColorSpace::YCCK, ColorSpace::RGB) => {
            color_convert_ycck_to_rgb::<3>(
                unprocessed,
                width,
                padded_width,
                output_colorspace,
                output,
            );
        }
        (ColorSpace::YCCK, ColorSpace::RGBA) => {
            color_convert_ycck_to_rgb::<4>(
                unprocessed,
                width,
                padded_width,
                output_colorspace,
                output,
            );
        }
        (ColorSpace::CMYK, ColorSpace::RGB) => {
            color_convert_cymk_to_rgb::<3>(unprocessed, width, padded_width, output);
        }
        (ColorSpace::CMYK, ColorSpace::RGBA) => {
            color_convert_cymk_to_rgb::<4>(unprocessed, width, padded_width, output);
        }
        (ColorSpace::MultiBand(n), _) => {
            if n != 2 {
                return Err(DecodeError::Format(format!(
                    "Unknown multiband sample ({n}), please share sample"
                )));
            }
            copy_removing_padding_generic(unprocessed, width, padded_width, output, n);
        }
        (ColorSpace::Luma, ColorSpace::RGB) => {
            convert_luma_to_rgb(unprocessed, width, padded_width, output)
        }
        (ColorSpace::Luma, ColorSpace::RGBA) => {
            convert_luma_to_rgba(unprocessed, width, padded_width, output)
        }
        _ => {
            let msg = format!(
                "Unimplemented colorspace mapping from {input_colorspace:?} to {output_colorspace:?}"
            );
            return Err(DecodeError::Format(msg));
        }
    }
    Ok(())
}
