//! 图像处理算子（当前仅实现仓库调用面需要的 `resize`）。

/// 重采样滤波器（镜像 image 0.25 的 FilterType）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterType {
    /// 最近邻。
    Nearest,
    /// 三角（双线性）核。
    Triangle,
    /// Catmull-Rom。
    CatmullRom,
    /// 高斯核。
    Gaussian,
    /// Lanczos3。
    Lanczos3,
    /// 三次样条。
    CubicSpline,
}

use crate::buffer::ImageBuffer;
use crate::traits::{Pixel, Primitive};
use crate::GenericImageView;

/// 通道标量的浮点转换辅助。
#[doc(hidden)]
pub trait FloatChannel: Primitive {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
}

impl FloatChannel for u8 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v.clamp(0.0, 255.0).round() as u8
    }
}
impl FloatChannel for u16 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v.clamp(0.0, 65535.0).round() as u16
    }
}
impl FloatChannel for f32 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(v: f64) -> Self {
        v.clamp(0.0, 1.0) as f32
    }
}

/// 滤波器核：窗口半径与卷积核函数。
struct FilterKernel {
    radius: f64,
    filter: fn(f64) -> f64,
}

impl FilterKernel {
    fn for_type(t: FilterType) -> FilterKernel {
        match t {
            FilterType::Nearest => FilterKernel {
                radius: 0.5,
                filter: nearest_filter,
            },
            FilterType::Triangle => FilterKernel {
                radius: 1.0,
                filter: triangle,
            },
            FilterType::CatmullRom => FilterKernel {
                radius: 2.0,
                filter: catmullrom,
            },
            FilterType::Gaussian => FilterKernel {
                radius: 3.0,
                filter: gaussian,
            },
            FilterType::Lanczos3 => FilterKernel {
                radius: 3.0,
                filter: lanczos3,
            },
            FilterType::CubicSpline => FilterKernel {
                radius: 3.0,
                filter: cubic_spline,
            },
        }
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let pix = std::f64::consts::PI * x;
        pix.sin() / pix
    }
}

fn nearest_filter(x: f64) -> f64 {
    if x.abs() < 0.5 { 1.0 } else { 0.0 }
}

fn triangle(x: f64) -> f64 {
    if x.abs() < 1.0 {
        1.0 - x.abs()
    } else {
        0.0
    }
}

fn catmullrom(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        1.5 * x * x * x - 2.5 * x * x + 1.0
    } else if x < 2.0 {
        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
    } else {
        0.0
    }
}

fn gaussian(x: f64) -> f64 {
    (-x * x / 2.0).exp()
}

fn lanczos3(x: f64) -> f64 {
    let x = x.abs();
    if x < 3.0 {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

fn cubic_spline(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        let a = x;
        (a + 1.0) * (a + 1.0) * (a + 1.0) / 6.0
            - 2.0 * a * a * a / 6.0
            + (a * a * a / 6.0 - a) / 2.0
    } else if x < 2.0 {
        let a = x;
        (2.0 - a) * (2.0 - a) * (2.0 - a) / 6.0
    } else {
        0.0
    }
}

/// 由源尺寸与目标尺寸计算实际输出尺寸（保持比例，按目标放宽/收缩）。
pub(crate) fn resize_dimensions(width: u32, height: u32, nwidth: u32, nheight: u32) -> (u32, u32) {
    let wratio = f64::from(nwidth) / f64::from(width);
    let hratio = f64::from(nheight) / f64::from(height);
    let ratio = wratio.min(hratio);
    let nw = f64::from(width) * ratio;
    let nh = f64::from(height) * ratio;
    (nw.round() as u32, nh.round() as u32)
}

/// 通用滤波器缩放（与 image 0.25 的语义一致：先水平再垂直的可分离卷积）。
pub fn resize<I: GenericImageView>(
    image: &I,
    nwidth: u32,
    nheight: u32,
    filter: FilterType,
) -> ImageBuffer<I::Pixel, Vec<<I::Pixel as Pixel>::Subpixel>>
where
    I::Pixel: Pixel<Subpixel: FloatChannel>,
{
    let (width, height) = image.dimensions();
    // 空源语义与 image 0.25 一致：直接返回「请求尺寸」的零图（避免除零与超大中间分配）。
    if width == 0 || height == 0 {
        return ImageBuffer::new(nwidth, nheight);
    }
    let (nwidth, nheight) = resize_dimensions(width, height, nwidth, nheight);
    if nwidth == 0 || nheight == 0 {
        return ImageBuffer::new(nwidth, nheight);
    }

    resize_fast(image, nwidth, nheight, FilterKernel::for_type(filter))
}

/// 可分离重采样：先生成水平采样表（复用各通道），再逐输出行做两次一维卷积。
fn resize_fast<I: GenericImageView>(
    image: &I,
    width: u32,
    height: u32,
    kernel: FilterKernel,
) -> ImageBuffer<I::Pixel, Vec<<I::Pixel as Pixel>::Subpixel>>
where
    I::Pixel: Pixel<Subpixel: FloatChannel>,
{
    let (src_w, src_h) = image.dimensions();
    let channels = usize::from(I::Pixel::CHANNEL_COUNT);
    let output_width = usize::try_from(width).unwrap();
    let output_height = usize::try_from(height).unwrap();
    let in_width = usize::try_from(src_w).unwrap();
    let in_height = usize::try_from(src_h).unwrap();

    // 水平采样表：每个输出列 → (起始源列, 权重, …)。
    let mut h_taps: Vec<Vec<(usize, f64)>> = Vec::with_capacity(output_width);
    let coef_count = (kernel.radius * 2.0).ceil() as usize + 1;
    for out in 0..output_width {
        let srcx = (out as f64 + 0.5) * in_width as f64 / width as f64 - 0.5;
        let mut sum = 0.0;
        h_taps.push(Vec::new());
        let start = (srcx - kernel.radius).floor() as i64;
        for i in start..start + coef_count as i64 {
            if i < 0 || (i as usize) >= in_width {
                continue;
            }
            let c = (kernel.filter)(i as f64 - srcx);
            if c == 0.0 {
                continue;
            }
            h_taps.last_mut().unwrap().push((i as usize, c));
            sum += c;
        }
        if sum != 0.0 {
            let last = h_taps.last_mut().unwrap();
            for e in last.iter_mut() {
                e.1 /= sum;
            }
        }
    }

    // 第一遍：水平卷积 → 中间行（height 行 × 输出宽度 × 通道 float）。
    let mut horizontal: Vec<f64> = vec![0.0; output_width * channels * in_height];
    for y in 0..in_height {
        for (out, taps) in h_taps.iter().enumerate() {
            for (in_x, w) in taps {
                let px = image.get_pixel(*in_x as u32, y as u32);
                let px = px.channels();
                for ch in 0..channels {
                    let v = FloatChannel::to_f64(px[ch]);
                    horizontal[(y * output_width + out) * channels + ch] += v * w;
                }
            }
        }
    }

    // 竖直采样表：支持高度可省略（卷积按需算）。
    let mut out_buf: Vec<<I::Pixel as Pixel>::Subpixel> =
        Vec::with_capacity(output_width * output_height * channels);
    let mut row = vec![0f64; output_width * channels];
    for out_y in 0..output_height {
        let srcy = (out_y as f64 + 0.5) * in_height as f64 / height as f64 - 0.5;
        let start = (srcy - kernel.radius).floor() as i64;
        let mut weights = Vec::new();
        let mut sum = 0.0;
        for i in start..start + coef_count as i64 {
            if i < 0 || (i as usize) >= in_height {
                continue;
            }
            let c = (kernel.filter)(i as f64 - srcy);
            if c == 0.0 {
                continue;
            }
            weights.push((i as usize, c));
            sum += c;
        }
        if sum != 0.0 {
            for e in weights.iter_mut() {
                e.1 /= sum;
            }
        }
        row.iter_mut().for_each(|v| *v = 0.0);
        for (in_y, w) in &weights {
            for (i, v) in row.iter_mut().enumerate() {
                *v += horizontal[(in_y * output_width) * channels + i] * w;
            }
        }
        for v in &row {
            out_buf.push(<I::Pixel as Pixel>::Subpixel::from_f64(*v));
        }
    }

    ImageBuffer::from_raw(width, height, out_buf)
        .expect("output buffer sized correctly")
}
