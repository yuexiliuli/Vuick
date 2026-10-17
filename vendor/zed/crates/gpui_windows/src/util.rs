use std::sync::OnceLock;

use anyhow::Context;
use gpui_util::ResultExt;
use windows::{
    UI::{
        Color,
        ViewManagement::{UIColorType, UISettings},
    },
    Win32::{
        Foundation::*, Graphics::Dwm::*, System::LibraryLoader::LoadLibraryA,
        UI::WindowsAndMessaging::*,
    },
    core::{BOOL, PCSTR},
};

use crate::*;
use gpui::*;

pub(crate) trait HiLoWord {
    fn hiword(&self) -> u16;
    fn loword(&self) -> u16;
    fn signed_hiword(&self) -> i16;
    fn signed_loword(&self) -> i16;
}

impl HiLoWord for WPARAM {
    fn hiword(&self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    fn loword(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn signed_hiword(&self) -> i16 {
        ((self.0 >> 16) & 0xFFFF) as i16
    }

    fn signed_loword(&self) -> i16 {
        (self.0 & 0xFFFF) as i16
    }
}

impl HiLoWord for LPARAM {
    fn hiword(&self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    fn loword(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn signed_hiword(&self) -> i16 {
        ((self.0 >> 16) & 0xFFFF) as i16
    }

    fn signed_loword(&self) -> i16 {
        (self.0 & 0xFFFF) as i16
    }
}

pub(crate) unsafe fn get_window_long(hwnd: HWND, nindex: WINDOW_LONG_PTR_INDEX) -> isize {
    #[cfg(target_pointer_width = "64")]
    unsafe {
        GetWindowLongPtrW(hwnd, nindex)
    }
    #[cfg(target_pointer_width = "32")]
    unsafe {
        GetWindowLongW(hwnd, nindex) as isize
    }
}

pub(crate) unsafe fn set_window_long(
    hwnd: HWND,
    nindex: WINDOW_LONG_PTR_INDEX,
    dwnewlong: isize,
) -> isize {
    #[cfg(target_pointer_width = "64")]
    unsafe {
        SetWindowLongPtrW(hwnd, nindex, dwnewlong)
    }
    #[cfg(target_pointer_width = "32")]
    unsafe {
        SetWindowLongW(hwnd, nindex, dwnewlong as i32) as isize
    }
}

pub(crate) fn windows_credentials_target_name(url: &str) -> String {
    format!("zed:url={}", url)
}

pub(crate) fn load_cursor(style: CursorStyle) -> Option<HCURSOR> {
    static ARROW: OnceLock<SafeCursor> = OnceLock::new();
    static IBEAM: OnceLock<SafeCursor> = OnceLock::new();
    static CROSS: OnceLock<SafeCursor> = OnceLock::new();
    static HAND: OnceLock<SafeCursor> = OnceLock::new();
    static SIZEWE: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENS: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENWSE: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENESW: OnceLock<SafeCursor> = OnceLock::new();
    static NO: OnceLock<SafeCursor> = OnceLock::new();
    let (lock, name) = match style {
        CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout => (&IBEAM, IDC_IBEAM),
        CursorStyle::Crosshair => (&CROSS, IDC_CROSS),
        CursorStyle::PointingHand | CursorStyle::DragLink => (&HAND, IDC_HAND),
        CursorStyle::ResizeLeft
        | CursorStyle::ResizeRight
        | CursorStyle::ResizeLeftRight
        | CursorStyle::ResizeColumn => (&SIZEWE, IDC_SIZEWE),
        CursorStyle::ResizeUp
        | CursorStyle::ResizeDown
        | CursorStyle::ResizeUpDown
        | CursorStyle::ResizeRow => (&SIZENS, IDC_SIZENS),
        CursorStyle::ResizeUpLeftDownRight => (&SIZENWSE, IDC_SIZENWSE),
        CursorStyle::ResizeUpRightDownLeft => (&SIZENESW, IDC_SIZENESW),
        CursorStyle::OperationNotAllowed => (&NO, IDC_NO),
        _ => (&ARROW, IDC_ARROW),
    };
    Some(
        *(*lock.get_or_init(|| {
            HCURSOR(
                unsafe { LoadImageW(None, name, IMAGE_CURSOR, 0, 0, LR_DEFAULTSIZE | LR_SHARED) }
                    .log_err()
                    .unwrap_or_default()
                    .0,
            )
            .into()
        })),
    )
}

/// This function is used to configure the dark mode for the window built-in title bar.
pub(crate) fn configure_dwm_dark_mode(hwnd: HWND, appearance: WindowAppearance) {
    let dark_mode_enabled: BOOL = match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => true.into(),
        WindowAppearance::Light | WindowAppearance::VibrantLight => false.into(),
    };
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_mode_enabled as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        )
        .log_err();
    }
}

#[inline]
pub(crate) fn logical_point(x: f32, y: f32, scale_factor: f32) -> Point<Pixels> {
    Point {
        x: px(x / scale_factor),
        y: px(y / scale_factor),
    }
}

// https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/apply-windows-themes
#[inline]
pub(crate) fn system_appearance() -> Result<WindowAppearance> {
    let ui_settings = UISettings::new()?;
    let foreground_color = ui_settings.GetColorValue(UIColorType::Foreground)?;
    // If the foreground is light, then is_color_light will evaluate to true,
    // meaning Dark mode is enabled.
    if is_color_light(&foreground_color) {
        Ok(WindowAppearance::Dark)
    } else {
        Ok(WindowAppearance::Light)
    }
}

#[inline(always)]
fn is_color_light(color: &Color) -> bool {
    ((5 * color.G as u32) + (2 * color.R as u32) + color.B as u32) > (8 * 128)
}

pub(crate) fn with_dll_library<R, F>(dll_name: PCSTR, f: F) -> Result<R>
where
    F: FnOnce(HMODULE) -> Result<R>,
{
    let library = unsafe {
        LoadLibraryA(dll_name).with_context(|| format!("Loading dll: {}", dll_name.display()))?
    };
    let result = f(library);
    unsafe {
        FreeLibrary(library)
            .with_context(|| format!("Freeing dll: {}", dll_name.display()))
            .log_err();
    }
    result
}

/// NV12（BT.709 有限范围）→ BGRA 的纯 CPU 参考实现，作为 GPU 像素着色器
/// （`external_yuv_fragment`）的对照基准。输入 Y/U/V（NV12 里 U=Cb、V=Cr），
/// 输出 `(B, G, R)` 各 8bit。
///
/// 目前仅供 `#[cfg(test)]` 单测做向量校验；将来视频 crate 的软 BGRA 回退路径
/// 需要时再取消该 cfg 门控导出。
#[cfg(test)]
pub fn yuv_nv12_to_bgra(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = y as f32;
    let u = u as f32;
    let v = v as f32;
    // BT.709 有限范围：Y ∈ [16,235]，Cb/Cr 零漂在 128
    let y_ = y - 16.0;
    let cb = u - 128.0;
    let cr = v - 128.0;
    let r = 1.164 * y_ + 1.596 * cr;
    let g = 1.164 * y_ - 0.391 * cr - 0.813 * cb;
    let b = 1.164 * y_ + 2.018 * cb;
    (clamp_channel(b), clamp_channel(g), clamp_channel(r))
}

#[inline]
#[cfg(test)]
fn clamp_channel(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    // 注意：不能用 `use super::*` —— 父模块带 `Foundation::*`/`WindowsAndMessaging::*`
    // 巨型 glob 导入，会撞 rustc（1.96 / edition 2024）`#[test]` 展开的递归限制。
    use super::yuv_nv12_to_bgra;

    #[test]
    fn test_yuv_nv12_to_bgra_known_vectors() {
        // BT.709 有限范围基准向量
        // 黑：Y=16、U=V=128 → RGB 全 0
        assert_eq!(yuv_nv12_to_bgra(16, 128, 128), (0, 0, 0));
        // 白：Y=235、U=V=128 → RGB 全 255
        assert_eq!(yuv_nv12_to_bgra(235, 128, 128), (255, 255, 255));
        // 红（BT.709 红色条 Y=81、U=90、V=240）→ (B, G, R) ≈ (0, 63, 254)
        assert_eq!(yuv_nv12_to_bgra(81, 90, 240), (0, 63, 254));
        // 蓝（Y=41、U=240、V=110）→ (B, G, R) = (255, 0, 0)
        assert_eq!(yuv_nv12_to_bgra(41, 240, 110), (255, 0, 0));
    }
}
