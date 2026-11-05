//! 上采样（1:1 复刻 zune-jpeg `upsampler/scalar.rs` 与 `worker.rs` 的上采样部分）。
//!
//! AVX2 上采样与标量字节恒等（zune 自身测试保证），故统一采用标量实现。

use crate::codecs::jpeg::error::DecodeError;
use crate::codecs::jpeg::segment::{Component, SampleRatios};

/// 上采样函数签名。
pub(crate) type UpSampler = fn(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    scratch_space: &mut [i16],
    output: &mut [i16],
);

/// 水平 2x 双线性上采样。
pub(crate) fn upsample_horizontal(
    input: &[i16],
    _input_ref: &[i16],
    _in_near: &[i16],
    _scratch: &mut [i16],
    output: &mut [i16],
) {
    debug_assert_eq!(input.len() * 2, output.len());
    debug_assert!(output.len() > 4 && input.len() > 2);

    output[0] = input[0];
    output[1] = (input[0] * 3 + input[1] + 2) >> 2;

    for (output_window, input_window) in output[2..].chunks_exact_mut(2).zip(input.windows(3)) {
        let sample = 3 * input_window[1] + 2;

        output_window[0] = (sample + input_window[0]) >> 2;
        output_window[1] = (sample + input_window[2]) >> 2;
    }

    let out_len = output.len() - 2;
    let input_len = input.len() - 2;

    let f_out = &mut output[out_len..];
    let i_last = &input[input_len..];

    f_out[0] = (3 * i_last[1] + i_last[0] + 2) >> 2;
    f_out[1] = i_last[1];
}

/// 垂直 2x 双线性上采样。
pub(crate) fn upsample_vertical(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    _scratch_space: &mut [i16],
    output: &mut [i16],
) {
    debug_assert_eq!(input.len() * 2, output.len());
    debug_assert_eq!(in_near.len(), input.len());
    debug_assert_eq!(in_far.len(), input.len());

    let middle = output.len() / 2;

    let (out_top, out_bottom) = output.split_at_mut(middle);

    for ((near, far), x) in input.iter().zip(in_near.iter()).zip(out_top) {
        *x = (((3 * near) + 2) + far) >> 2;
    }
    for ((near, far), x) in input.iter().zip(in_far.iter()).zip(out_bottom) {
        *x = (((3 * near) + 2) + far) >> 2;
    }
}

/// 4:2:0（HV）2x2 平均上采样：先垂直后水平。
pub(crate) fn upsample_hv(
    input: &[i16],
    in_near: &[i16],
    in_far: &[i16],
    scratch_space: &mut [i16],
    output: &mut [i16],
) {
    debug_assert_eq!(input.len() * 4, output.len());
    debug_assert!(input.len() * 2 <= scratch_space.len());
    let scratch_space = &mut scratch_space[..input.len() * 2];

    let mut t = [0];
    upsample_vertical(input, in_near, in_far, &mut t, scratch_space);

    let scratch_half = scratch_space.len() / 2;
    let output_half = output.len() / 2;

    upsample_horizontal(
        &scratch_space[..scratch_half],
        &[],
        &[],
        &mut t,
        &mut output[..output_half],
    );

    upsample_horizontal(
        &scratch_space[scratch_half..],
        &[],
        &[],
        &mut t,
        &mut output[output_half..],
    );
}

/// 通用比例上采样（最近邻）。
pub(crate) fn upsample_generic(
    input: &[i16],
    _in_near: &[i16],
    _in_far: &[i16],
    _scratch_space: &mut [i16],
    output: &mut [i16],
) {
    let difference = output.len() / input.len();
    if difference > 0 {
        for (input, chunk_output) in input.iter().zip(output.chunks_exact_mut(difference)) {
            chunk_output.iter_mut().for_each(|x| *x = *input);
        }
    }
}

/// 无操作上采样。
pub(crate) fn upsample_no_op(
    _input: &[i16],
    _in_near: &[i16],
    _in_far: &[i16],
    _scratch_space: &mut [i16],
    _output: &mut [i16],
) {
}

/// 单个分量一行的上采样装配（对应 zune `worker::upsample`）。
///
/// 处理 MCU 行边界：HV/V 需要保存上一行供下次使用，H 只需逐行水平复制。
pub(crate) fn upsample(
    component: &mut Component,
    mcu_height: usize,
    i: usize,
    upsampler_scratch_space: &mut [i16],
    has_vertical_sample: bool,
) -> Result<(), DecodeError> {
    match component.sample_ratio {
        SampleRatios::V | SampleRatios::HV => {
            let mut dest_start = 0;
            let stride_bytes_written = component.width_stride * component.sample_ratio.sample();

            if i > 0 {
                let stride = component.width_stride;

                let dest = &mut component.first_row_upsample_dest[0..stride_bytes_written];

                let row = &component.row[..];
                let row_up = &component.row_up[..];
                let row_down = &component.raw_coeff[0..stride];
                (component.up_sampler)(row, row_up, row_down, upsampler_scratch_space, dest);
            }

            let mut upsample = true;

            let stride = component.width_stride * component.vertical_sample;
            let stop_offset = component.raw_coeff.len() / component.width_stride;

            if component.raw_coeff.len() != stop_offset * stride {
                return Err(DecodeError::Format(
                    "Invalid component dimensions, would panic".to_string(),
                ));
            }
            for (pos, curr_row) in component
                .raw_coeff
                .chunks_exact(component.width_stride)
                .enumerate()
            {
                let mut dest: &mut [i16] = &mut [];
                let mut row_up: &[i16] = &[];
                let mut row_down: &[i16] = &[];

                if i == 0 && pos == 0 {
                    row_up = &component.raw_coeff[pos * stride..(pos + 1) * stride];
                    row_down = &component.raw_coeff[(pos + 1) * stride..(pos + 2) * stride];
                } else if i > 0 && pos == 0 {
                    row_up = &component.row[..];
                    row_down = &component.raw_coeff[(pos + 1) * stride..(pos + 2) * stride];
                } else if i == mcu_height.saturating_sub(1) && pos == stop_offset - 1 {
                    row_up = &component.raw_coeff[(pos - 1) * stride..pos * stride];
                    row_down = &component.raw_coeff[pos * stride..(pos + 1) * stride];
                } else if pos > 0 && pos < stop_offset - 1 {
                    row_up = &component.raw_coeff[(pos - 1) * stride..pos * stride];
                    row_down = &component.raw_coeff[(pos + 1) * stride..(pos + 2) * stride];
                } else if pos == stop_offset - 1 {
                    let prev_row = &component.raw_coeff[(pos - 1) * stride..pos * stride];

                    component.row_up.copy_from_slice(prev_row);
                    component.row.copy_from_slice(curr_row);
                    upsample = false;
                } else {
                    unreachable!("Uh oh!");
                }
                if upsample {
                    dest =
                        &mut component.upsample_dest[dest_start..dest_start + stride_bytes_written];
                    dest_start += stride_bytes_written;
                }

                if upsample {
                    (component.up_sampler)(
                        curr_row,
                        row_up,
                        row_down,
                        upsampler_scratch_space,
                        dest,
                    );
                }
            }
        }
        SampleRatios::H => {
            component
                .upsample_dest
                .resize(component.raw_coeff.len() * 2, 0);

            let raw_coeff = &component.raw_coeff;
            let dest_coeff = &mut component.upsample_dest;

            if has_vertical_sample {
                let length = component.first_row_upsample_dest.len();
                component
                    .first_row_upsample_dest
                    .copy_from_slice(&dest_coeff.rchunks_exact(length).next().unwrap());
            }
            for (single_row, output_stride) in raw_coeff
                .chunks_exact(component.width_stride)
                .zip(dest_coeff.chunks_exact_mut(component.width_stride * 2))
            {
                (component.up_sampler)(single_row, &[], &[], &mut [], output_stride);
            }
        }
        SampleRatios::Generic(h, v) => {
            let raw_coeff = &component.raw_coeff;
            let dest_coeff = &mut component.upsample_dest;

            for (single_row, output_stride) in raw_coeff
                .chunks_exact(component.width_stride)
                .zip(dest_coeff.chunks_exact_mut(component.width_stride * h * v))
            {
                for row in output_stride.chunks_exact_mut(component.width_stride * h) {
                    (component.up_sampler)(single_row, &[], &[], &mut [], row);
                }
            }
        }
        SampleRatios::None => {}
    };
    Ok(())
}
