//! 动画支持：`Frame` / `Delay` / `Frames` / `AnimationDecoder`（镜像 image 0.25 语义）。

use std::cmp::Ordering;
use std::time::Duration;

use crate::buffer::RgbaImage;
use crate::error::ImageResult;

/// 帧延迟：以「毫秒分数」ratio 存储，构造时约分，保证不同进制间可比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd)]
pub struct Delay {
    ratio: Ratio,
}

/// 单个动画帧。
#[derive(Clone)]
pub struct Frame {
    delay: Delay,
    left: u32,
    top: u32,
    buffer: RgbaImage,
}

/// 实现相关的帧迭代器。
pub struct Frames<'a> {
    iterator: Box<dyn Iterator<Item = ImageResult<Frame>> + 'a>,
}

/// 动画解码器 trait。
pub trait AnimationDecoder<'a> {
    /// 消费解码器产出一系列帧。
    fn into_frames(self) -> Frames<'a>;

    /// 动画循环次数（默认 1 次）。
    fn loop_count(&self) -> LoopCount {
        LoopCount::Finite(core::num::NonZeroU32::new(1).unwrap())
    }
}

impl<'a> Frames<'a> {
    /// 由实现相关的迭代器构造。
    pub fn new(iterator: Box<dyn Iterator<Item = ImageResult<Frame>> + 'a>) -> Self {
        Frames { iterator }
    }

    /// 走完迭代器收集所有帧；遇错立即返回。
    pub fn collect_frames(self) -> ImageResult<Vec<Frame>> {
        self.collect()
    }
}

impl Iterator for Frames<'_> {
    type Item = ImageResult<Frame>;

    fn next(&mut self) -> Option<ImageResult<Frame>> {
        self.iterator.next()
    }
}

impl Frame {
    /// 无延迟、无偏移地构造一帧。
    pub fn new(buffer: RgbaImage) -> Frame {
        Frame {
            delay: Delay::from_ratio(Ratio { numer: 0, denom: 1 }),
            left: 0,
            top: 0,
            buffer,
        }
    }

    /// 带偏移与延迟构造。
    pub fn from_parts(buffer: RgbaImage, left: u32, top: u32, delay: Delay) -> Frame {
        Frame {
            delay,
            left,
            top,
            buffer,
        }
    }

    /// 本帧延迟。
    pub fn delay(&self) -> Delay {
        self.delay
    }

    /// 位图只读引用。
    pub fn buffer(&self) -> &RgbaImage {
        &self.buffer
    }

    /// 位图可变引用。
    pub fn buffer_mut(&mut self) -> &mut RgbaImage {
        &mut self.buffer
    }

    /// 取出位图。
    pub fn into_buffer(self) -> RgbaImage {
        self.buffer
    }

    /// 左偏移。
    pub fn left(&self) -> u32 {
        self.left
    }

    /// 上偏移。
    pub fn top(&self) -> u32 {
        self.top
    }
}

impl Delay {
    /// 以毫秒分数构造延迟（构造时约分）。
    pub fn from_numer_denom_ms(numerator: u32, denominator: u32) -> Self {
        Delay {
            ratio: Ratio::new(numerator, denominator),
        }
    }

    /// 从 `Duration` 构造。
    pub fn from_saturating_duration(duration: Duration) -> Self {
        let ms = u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .min(u64::from(u32::MAX)) as u32;
        Delay::from_ratio(Ratio::new(ms, 1))
    }

    /// 恢复为毫秒分数（约分后的值）。
    pub fn numer_denom_ms(self) -> (u32, u32) {
        (self.ratio.numer, self.ratio.denom)
    }

    pub(crate) fn from_ratio(ratio: Ratio) -> Self {
        Delay { ratio }
    }

    pub(crate) fn into_ratio(self) -> Ratio {
        self.ratio
    }
}

impl From<Delay> for Duration {
    fn from(delay: Delay) -> Self {
        let ratio = delay.into_ratio();
        let ms = ratio.to_integer();
        let rest = ratio.numer % ratio.denom;
        let nanos = (u64::from(rest) * 1_000_000) / u64::from(ratio.denom);
        Duration::from_millis(u64::from(ms)) + Duration::from_nanos(nanos)
    }
}

/// 最简分数（构造时约分）。
#[derive(Copy, Clone, Debug)]
pub(crate) struct Ratio {
    numer: u32,
    denom: u32,
}

impl Ratio {
    pub(crate) fn new(numerator: u32, denominator: u32) -> Self {
        assert_ne!(denominator, 0);
        let divisor = gcd(numerator, denominator);
        Self {
            numer: numerator / divisor,
            denom: denominator / divisor,
        }
    }

    pub(crate) fn to_integer(self) -> u32 {
        self.numer / self.denom
    }
}

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a.rem_euclid(b));
    }
    a
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> Ordering {
        (u64::from(self.numer) * u64::from(other.denom))
            .cmp(&(u64::from(other.numer) * u64::from(self.denom)))
    }
}

/// 动画循环次数。
#[derive(Clone, Copy)]
pub enum LoopCount {
    /// 无限循环。
    Infinite,
    /// 有限次数。
    Finite(core::num::NonZeroU32),
}
