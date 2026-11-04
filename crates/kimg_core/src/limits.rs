//! 解码资源限制（镜像 image 0.25 的 `Limits` 语义，防解压炸弹）。

use crate::color::ColorType;
use crate::error::{ImageError, ImageResult, LimitError, LimitErrorKind};

/// 解码器声明支持哪些严格限制（本实现仅严格支持尺寸限制）。
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct LimitSupport {}

/// 资源限制。默认值：仅 `max_alloc = 512 MiB`（与 image 0.25.10 一致，宽高无默认上限）。
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Limits {
    /// 最大允许宽度（严格限制）。
    pub max_image_width: Option<u32>,
    /// 最大允许高度（严格限制）。
    pub max_image_height: Option<u32>,
    /// 解码器单次分配上限（软限制）。默认 512 MiB。
    pub max_alloc: Option<u64>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_image_width: None,
            max_image_height: None,
            max_alloc: Some(512 * 1024 * 1024),
        }
    }
}

impl Limits {
    /// 关闭全部限制。
    pub fn no_limits() -> Limits {
        Limits {
            max_image_width: None,
            max_image_height: None,
            max_alloc: None,
        }
    }

    /// 校验当前严格限制是否被支持（本实现全部限制均支持，恒成功）。
    pub fn check_support(&self, _supported: &LimitSupport) -> ImageResult<()> {
        Ok(())
    }

    /// 校验宽高是否超过严格限制。
    pub fn check_dimensions(&self, width: u32, height: u32) -> ImageResult<()> {
        if let Some(max_width) = self.max_image_width {
            if width > max_width {
                return Err(ImageError::Limits(LimitError::from_kind(
                    LimitErrorKind::DimensionError,
                )));
            }
        }
        if let Some(max_height) = self.max_image_height {
            if height > max_height {
                return Err(ImageError::Limits(LimitError::from_kind(
                    LimitErrorKind::DimensionError,
                )));
            }
        }
        Ok(())
    }

    /// 从 `max_alloc` 中扣减 `amount` 字节；超出返回内存不足错误。
    pub fn reserve(&mut self, amount: u64) -> ImageResult<()> {
        if let Some(max_alloc) = self.max_alloc.as_mut() {
            if *max_alloc < amount {
                return Err(ImageError::Limits(LimitError::from_kind(
                    LimitErrorKind::InsufficientMemory,
                )));
            }
            *max_alloc -= amount;
        }
        Ok(())
    }

    /// `reserve` 的 usize 便捷版本。
    pub fn reserve_usize(&mut self, amount: usize) -> ImageResult<()> {
        match u64::try_from(amount) {
            Ok(n) => self.reserve(n),
            Err(_) if self.max_alloc.is_some() => Err(ImageError::Limits(
                LimitError::from_kind(LimitErrorKind::InsufficientMemory),
            )),
            Err(_) => Ok(()),
        }
    }

    /// 按宽高与颜色类型一次算好内存占用并 `reserve`。
    pub fn reserve_buffer(
        &mut self,
        width: u32,
        height: u32,
        color_type: ColorType,
    ) -> ImageResult<()> {
        self.check_dimensions(width, height)?;
        let in_memory_size = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(color_type.bytes_per_pixel().into());
        self.reserve(in_memory_size)?;
        Ok(())
    }

    /// 归还 `amount` 字节到 `max_alloc`。
    pub fn free(&mut self, amount: u64) {
        if let Some(max_alloc) = self.max_alloc.as_mut() {
            *max_alloc = max_alloc.saturating_add(amount);
        }
    }

    /// `free` 的 usize 便捷版本。
    pub fn free_usize(&mut self, amount: usize) {
        match u64::try_from(amount) {
            Ok(n) => self.free(n),
            Err(_) if self.max_alloc.is_some() => {
                panic!("max_alloc is set, we should have exited earlier when the reserve failed")
            }
            Err(_) => {}
        }
    }
}
