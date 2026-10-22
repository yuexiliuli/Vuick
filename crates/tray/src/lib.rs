//! 系统托盘 crate：把「关闭到系统托盘」相关的 unsafe Win32 调用**全部收敛在本 crate**，
//! 对外暴露**线程安全**的纯函数式 API（内部全部模块级 static / 原子，无裸 `static mut`）。
//!
//! 行为（Windows）：
//! - 注册一个**隐藏消息窗口**（按 UI 线程创建、从不 `ShowWindow`）；托盘回调消息经
//!   gpui 主循环既有 `GetMessageW`/`DispatchMessageW` 派发到该窗口的 `WndProc`；
//! - `Shell_NotifyIconW` 创建/移除托盘图标（幂等）：左键（单击/双击）恢复主窗口置前、
//!   右键 `TrackPopupMenu` 弹「恢复主窗口 / 退出」菜单；
//! - `should_close_to_tray()` 供主窗口 `on_window_should_close` 回调判定：开关开、主窗口
//!   已知、且无「强制真退出」标记 → 隐藏到托盘（返回 false 拦下关闭）；否则放行。
//!
//! 非 `windows` 目标一律编译为 no-op（不破坏跨平台构建）。
//!
//! 真机托盘行为（隐藏/恢复/菜单）无法 headless 验证 → 本 crate 测试只覆盖可单测的
//! **纯 flag 逻辑**（R2），测试构建绝不真建托盘图标 / 窗口 / 投递窗口消息（K21）。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// 内嵌程序图标资源（288×288 @8bpp 调色板 PNG；被 git 跟踪的入库资产，位于本文件
/// 上级两级 `crates/ui/assets/app-icon-228x228.png`）。解码后缩到 64×64 构造 HICON。
/// Cargo 不追踪包外 `include_bytes!` 的改动 → 换图标需 touch 本 crate 触发重建。
const APP_ICO: &[u8] = include_bytes!("../../ui/assets/app-icon-228x228.png");

/// 主窗口 HWND（以 `isize` 承载，0 = 未知；由 shell 在窗口建立后写入）。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
/// 程序图标 HICON（内嵌 PNG 解码变色龙，`isize` 承载；0 = 未创建）。
/// 窗口图标与托盘图标**共用**这一份：窗口 / 托盘存续期内都保持有效，
/// 随 [`shutdown`] 释放（先清托盘、再 DestroyIcon 归零）。
static WINDOW_ICON: AtomicIsize = AtomicIsize::new(0);
/// 「关闭到系统托盘」开关（文件菜单切换，随 `UserPrefs` 立即持久化；默认关）。
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);
/// 「强制真退出」一次性标记：托盘菜单「退出」置位后经 `WM_CLOSE` 关闭主窗口；
/// [`should_close_to_tray`] 读到该位返回 false 并**消费**它（下一次按开关状态判定）。
static FORCE_QUIT: AtomicBool = AtomicBool::new(false);

/// 记录主窗口 HWND（`isize` 承载的 HWND；0 = 未知）。线程安全。
pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, Ordering::SeqCst);
}

/// 翻转「关闭到系统托盘」开关：只翻录 flag（决定点 × / Alt+F4 是否隐藏到托盘），
/// **不**增删托盘图标——图标由 [`ensure_tray_icon`] 启动时创建、常驻（用户要求托盘
/// 图标始终显示为应用标志，与开关无关）。
pub fn set_close_to_tray(enabled: bool) {
    CLOSE_TO_TRAY.store(enabled, Ordering::SeqCst);
}

/// 确保应用托盘图标常驻（启动时调用，幂等）：图标**始终显示**（不随「关闭到系统
/// 托盘」开关消失，开关只决定点 × 是否隐藏窗口）。测试构建不真建托盘图标/窗口。
pub fn ensure_tray_icon() {
    #[cfg(windows)]
    if !cfg!(test) {
        let _ = imp::add_tray_icon();
    }
}

/// 判定「点 × / Alt+F4 是否应隐藏到托盘」（纯 flag 逻辑，可单测 R2）：
/// 开关开 **且** 主窗口已知 **且** 无强制退出标记 → true；否则 false。
/// 读取时**消费** `FORCE_QUIT` 一次性标记（托盘「退出」后放行一次真正关闭）。
pub fn should_close_to_tray() -> bool {
    if !CLOSE_TO_TRAY.load(Ordering::SeqCst) {
        return false;
    }
    if MAIN_HWND.load(Ordering::SeqCst) == 0 {
        return false;
    }
    !FORCE_QUIT.swap(false, Ordering::SeqCst)
}

/// 隐藏主窗口到任务栏托盘（`MAIN_HWND` 为 0 时 no-op）。
pub fn hide_main_window() {
    #[cfg(windows)]
    imp::hide_main_window();
}

/// 设置程序窗口图标（任务栏按钮 / Alt+Tab / 标题栏 logo）：对主窗口 HWND 发
/// `WM_SETICON`（ICON_BIG + ICON_SMALL），图标为内嵌 PNG 解码变色龙（[`crate::APP_ICO`]）。
/// UUID：主窗口未建立（HWND 未知）时 no-op；图标创建失败静默跳过（不 panic）。
/// 测试构建不投递真实窗口消息（`imp::set_window_icon` 内 `#[cfg(not(test))]`）。
pub fn set_window_icon() {
    #[cfg(windows)]
    imp::set_window_icon();
}

/// 请求真正退出：置「强制退出」标记 + 向主窗口投递 `WM_CLOSE`（主窗口
/// `should_close` 回调读到该标记 → 放行关闭，走正常退出路径）。
pub fn request_quit() {
    FORCE_QUIT.store(true, Ordering::SeqCst);
    #[cfg(windows)]
    imp::request_quit();
}

/// 优雅清理（`on_app_quit` 兜底钩子调用，弱要求）：移除托盘图标 + 销毁隐藏窗口
/// （进程退出系统也会自动清托盘图标，此为 best-effort）。
pub fn shutdown() {
    #[cfg(windows)]
    imp::shutdown();
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, DIB_RGB_COLORS, BI_RGB, BITMAPINFO,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
        NIM_DELETE, NIN_SELECT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DestroyIcon, DestroyWindow, GetCursorPos, LoadIconW, RegisterClassW,
        SetForegroundWindow, ShowWindow, TrackPopupMenu, HCURSOR, HICON, HMENU,
        ICONINFO, IDI_APPLICATION, MF_STRING, SW_RESTORE, TPM_LEFTALIGN, TPM_RIGHTBUTTON,
        WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WNDCLASS_STYLES, WM_APP, WM_COMMAND,
        WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_NULL};
    #[cfg(not(test))]
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, WM_CLOSE};

    /// 托盘回调消息号（`WM_APP + 1`）：`Shell_NotifyIconW` 把它投到隐藏窗口。
    const TRAY_MSG: u32 = WM_APP + 1;
    /// 托盘图标 ID。
    const TRAY_ICON_ID: u32 = 1;
    /// 托盘菜单命令号 1 = 恢复主窗口。
    const CMD_RESTORE: usize = 1;
    /// 托盘菜单命令号 2 = 退出。
    const CMD_QUIT: usize = 2;

    /// 隐藏消息窗口 HWND（0 = 未建）。
    static HIDDEN_HWND: AtomicIsize = AtomicIsize::new(0);
    /// 窗口类是否已注册（`RegisterClassW` 失败多半是同类已存在 → 幂等继续）。
    static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    /// 托盘右键菜单 HMENU（非 `Send`/`Sync` → 以 `isize` 承载，0 = 未建）。
    static TRAY_MENU: AtomicIsize = AtomicIsize::new(0);

    fn hidden_hwnd() -> HWND {
        HWND(HIDDEN_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
    }

    /// 确保隐藏消息窗口已注册并创建（幂等）；UI 线程调用（`set_close_to_tray` 触发，
    /// 即 gpui 主线程）。失败返回 Err（不 panic）。
    fn ensure_hidden_window() -> Result<HWND, ()> {
        if HIDDEN_HWND.load(Ordering::SeqCst) != 0 {
            return Ok(hidden_hwnd());
        }
        let class_name = w!("GpuiEditorTrayHidden");
        // 隐藏窗口创建：注册窗口类 + CreateWindowExW（Win32 调用收敛在本 unsafe 块）。
        let hwnd = unsafe {
            let hmodule: HMODULE = GetModuleHandleW(None).map_err(|_| ())?;
            let hinst = hmodule.into();
            if !CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
                let wc = WNDCLASSW {
                    style: WNDCLASS_STYLES(0),
                    lpfnWndProc: Some(hidden_wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: HICON(std::ptr::null_mut()),
                    hCursor: HCURSOR(std::ptr::null_mut()),
                    hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
                    lpszMenuName: PCWSTR::null(),
                    lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
                };
                // 同类已存在（重复注册）→ 失败无妨，继续创建窗口（幂等语义）。
                let _ = RegisterClassW(&wc);
            }
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR::from_raw(class_name.as_ptr()),
                PCWSTR::from_raw(w!("").as_ptr()),
                WINDOW_STYLE(0), // WS_OVERLAPPED：从不 ShowWindow，保持不可见隐藏窗口
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
            .map_err(|_| ())?
        };
        HIDDEN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        ensure_menu();
        Ok(hwnd)
    }

    /// 创建托盘右键菜单（幂等，仅一次；无效则静默失败，不 panic）。
    fn ensure_menu() {
        if TRAY_MENU.load(Ordering::SeqCst) != 0 {
            return;
        }
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let ok = AppendMenuW(
                menu,
                MF_STRING,
                CMD_RESTORE,
                PCWSTR::from_raw(w!("恢复主窗口").as_ptr()),
            )
            .is_ok()
                && AppendMenuW(
                    menu,
                    MF_STRING,
                    CMD_QUIT,
                    PCWSTR::from_raw(w!("退出").as_ptr()),
                )
                .is_ok();
            if !ok {
                let _ = DestroyMenu(menu);
                return;
            }
            TRAY_MENU.store(menu.0 as isize, Ordering::SeqCst);
        }
    }

    /// 托盘图标 szTip（宽字符，`\0` 结尾；长度截断留 1 个结尾空位）。
    fn fill_tip(buf: &mut [u16; 128]) {
        let mut i = 0;
        for u in "GPUI 编辑器".encode_utf16() {
            if i < buf.len() - 1 {
                buf[i] = u;
                i += 1;
            } else {
                break;
            }
        }
        buf[i] = 0;
    }

    /// 从内嵌 PNG 资源（288×288 调色板色）构造应用图标 HICON。
    ///
    /// 路线：`image` crate 解码 PNG（调色板自动展开 RGBA）→ 缩到 64×64（托盘/任务栏
    /// 常用档，W32 缩放质量最好且 GDI 位图小）→ BGRA straight →
    /// [`create_hicon_from_bgra`]（CreateDIBSection + CreateIconIndirect，先前真机验证可用）。
    fn create_hicon_from_ico() -> Result<HICON, ()> {
        let img = image::ImageReader::new(std::io::Cursor::new(crate::APP_ICO))
            .with_guessed_format()
            .map_err(|_| ())?
            .decode()
            .map_err(|_| ())?;
        // 288×288 原图缩到 64×64 构造 HICON（托盘/任务栏 ~16-48px 显示，系统缩放；
        // 更大的 HICON 在 W32 图标路径缩放质量/兼容性更差且 GDI 位图更大）。
        let rgba = img.to_rgba8();
        let small =
            image::imageops::resize(&rgba, 64, 64, image::imageops::FilterType::Lanczos3);
        let (w, h) = small.dimensions();
        debug_assert_eq!((w, h), (64, 64), "缩到 64 后尺寸不符");
        // RGBA → BGRA straight（Win32 DIB 以 BGRA 字节序消费）。
        let mut bgra = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for px in small.as_raw().chunks_exact(4) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        create_hicon_from_bgra(bgra, w, h).map_err(|_| ())
    }

    /// 从 BGRA straight 像素构造 HICON：`CreateDIBSection`（底朝上 32bpp DIB）+
    /// `CreateIconIndirect`（HICON 是源位图的独立副本 → 返回后可删源位图）。
    /// 该路线在先前手绘图标版真机验证可用（托盘图标正常显示）；失败返回 Err 不 panic。
    fn create_hicon_from_bgra(bgra: Vec<u8>, w: u32, h: u32) -> windows::core::Result<HICON> {
        // SAFETY（Win32 机制）：
        // - `CreateDIBSection`（Gdi）：32bpp、正向 biHeight=底朝上，ppvBits 返回像素首址
        //   （DIB_RGB_COLORS 省略调色板）；把视觉第 y 行拷到 (h-1-y) 行。
        // - `CreateBitmap`：1bpp 全 0（AND 掩码不屏蔽，透明度完全交 alpha 通道）。
        // - `CreateIconIndirect` 复制两个源位图为图标资源 → 返回后 `DeleteObject` 删源
        //   （官方惯例，防 GDI 泄漏）。
        unsafe {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize =
                std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = h as i32;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbm_color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            debug_assert!(!bits.is_null());
            let stride = (w as usize) * 4;
            for y in 0..(h as usize) {
                let src = &bgra[y * stride..(y + 1) * stride];
                let dst = (bits as *mut u8).add((h as usize - 1 - y) * stride);
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst, stride);
            }
            // AND 掩码：1bpp 全 0，行按 32 位字对齐（ceil(w/32)*4 字节/行）。
            let mask_len = ((((w as usize) + 31) / 32) * 4) * (h as usize);
            let mask: Vec<u8> = vec![0u8; mask_len];
            let hbm_mask = CreateBitmap(
                w as i32,
                h as i32,
                1,
                1,
                Some(mask.as_ptr() as *const core::ffi::c_void),
            );
            let info = ICONINFO {
                fIcon: windows::core::BOOL(1),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: hbm_mask,
                hbmColor: hbm_color,
            };
            let result = CreateIconIndirect(&info);
            let _ = DeleteObject(hbm_color.into());
            let _ = DeleteObject(hbm_mask.into());
            result
        }
    }

    /// 确保程序图标 HICON 已创建（幂等，窗口 / 托盘共用）。返回 `isize` 承载的 HICON；
    /// 失败返回 Err（不 panic）。仅由 UI 线程调用（`set_window_icon` / `add_tray_icon`）。
    pub(super) fn ensure_window_icon() -> Result<isize, ()> {
        let existing = super::WINDOW_ICON.load(Ordering::SeqCst);
        if existing != 0 {
            return Ok(existing);
        }
        // SAFETY：`create_hicon_from_ico` 内部 FFI 依据见其文档注释（一次性构造）。
        // ICO 加载失败 → Err 走调用方静默跳过（回退系统默认图标，不 panic）。
        let hicon = create_hicon_from_ico().map_err(|_| ())?;
        let raw = hicon.0 as isize;
        // 两个调用点（set_window_icon / add_tray_icon）都在 UI 线程串行，无并发 → 直接落址即可。
        super::WINDOW_ICON.store(raw, Ordering::SeqCst);
        Ok(raw)
    }

    /// 为窗口设置程序图标：`WM_SETICON`（ICON_BIG + ICON_SMALL）。测试构建不投递消息。
    pub(super) fn set_window_icon() {
        #[cfg(not(test))]
        {
            use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON};
            let h = super::MAIN_HWND.load(Ordering::SeqCst);
            if h == 0 {
                return;
            }
            let Ok(raw) = ensure_window_icon() else {
                return;
            };
            let wnd = HWND(h as *mut core::ffi::c_void);
            let hicon = HICON(raw as *mut core::ffi::c_void);
            // WM_SETICON 处理路径（已读 gpui_windows 源码确认）：
            // gpui 窗口 WndProc 链 = window.rs `window_procedure` → `handle_msg` →
            // events.rs 消息分发表，**WM_SETICON 不在其 match 列表**（events.rs `_ => None`），
            // 最终统一落 `DefWindowProcW`（events.rs:170）→ 由系统处理：按窗口保存图标并重绘
            // （ICON_BIG 驱动任务栏/Alt+Tab，ICON_SMALL 驱动标题栏/小尺寸图标）。
            // SendMessage 在 UI 线程、主窗口已建成后调用，无跨线程/消息重入问题。
            unsafe {
                SendMessageW(
                    wnd,
                    WM_SETICON,
                    Some(WPARAM(ICON_BIG as usize)),
                    Some(LPARAM(hicon.0 as isize)),
                );
                SendMessageW(
                    wnd,
                    WM_SETICON,
                    Some(WPARAM(ICON_SMALL as usize)),
                    Some(LPARAM(hicon.0 as isize)),
                );
            }
        }
    }

    /// 创建托盘图标（`NIM_ADD`，幂等：已建隐藏窗口则重复创建会失败，静默返回 false）。
    pub(super) fn add_tray_icon() -> bool {
        let Ok(hwnd) = ensure_hidden_window() else {
            return false;
        };
        // 优先用程序图标（内嵌 ICO 变色龙）；创建失败回退系统默认图标（保底，不 panic）。
        let hicon: HICON = match ensure_window_icon() {
            Ok(raw) => HICON(raw as *mut core::ffi::c_void),
            Err(()) => {
                let Ok(ic) = (unsafe { LoadIconW(None, IDI_APPLICATION) }) else {
                    return false;
                };
                ic
            }
        };
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = TRAY_MSG;
        data.hIcon = hicon;
        fill_tip(&mut data.szTip);
        unsafe { Shell_NotifyIconW(NIM_ADD, &data).as_bool() }
    }

    /// 移除托盘图标（`NIM_DELETE`，幂等：从未创建也返回 true）。
    pub(super) fn remove_tray_icon() -> bool {
        if HIDDEN_HWND.load(Ordering::SeqCst) == 0 {
            return true;
        }
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hidden_hwnd();
        data.uID = TRAY_ICON_ID;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data).as_bool() }
    }

    /// 隐藏主窗口（`MAIN_HWND` 为 0 时 no-op）。测试构建不真投递窗口消息。
    pub(super) fn hide_main_window() {
        #[cfg(not(test))]
        {
            let h = super::MAIN_HWND.load(Ordering::SeqCst);
            if h != 0 {
                let _ = unsafe { ShowWindow(HWND(h as *mut core::ffi::c_void), SW_HIDE) };
            }
        }
    }

    /// 请求真退出：向主窗口投递 `WM_CLOSE`（test 构建跳过，不放真实窗口消息）。
    pub(super) fn request_quit() {
        #[cfg(not(test))]
        {
            let h = super::MAIN_HWND.load(Ordering::SeqCst);
            if h != 0 {
                let _ = unsafe {
                    PostMessageW(
                        Some(HWND(h as *mut core::ffi::c_void)),
                        WM_CLOSE,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
        }
    }

    /// 优雅清理：移除图标 + 销毁隐藏窗口 + 销毁菜单 + 释放程序图标（幂等）。
    pub(super) fn shutdown() {
        let _ = remove_tray_icon();
        let h = HIDDEN_HWND.swap(0, Ordering::SeqCst);
        if h != 0 {
            let _ = unsafe { DestroyWindow(HWND(h as *mut core::ffi::c_void)) };
        }
        let m = TRAY_MENU.swap(0, Ordering::SeqCst);
        if m != 0 {
            let _ = unsafe { DestroyMenu(HMENU(m as *mut core::ffi::c_void)) };
        }
        let ic = super::WINDOW_ICON.swap(0, Ordering::SeqCst);
        if ic != 0 {
            // SAFETY：`DestroyIcon` 释放合法 HICON（WindowsAndMessaging/mod.rs）。
            let _ = unsafe { DestroyIcon(HICON(ic as *mut core::ffi::c_void)) };
        }
    }

    /// 隐藏窗口的窗口过程：托盘点按回调 / 菜单命令都经此分发。
    unsafe extern "system" fn hidden_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            // 托盘回调消息：`lParam` 是鼠标消息（旧版 Shell_NotifyIcon），
            // `wParam` 是图标 ID；兼容 `NIN_SELECT`（NOTIFYICON_VERSION_4 风格）。
            TRAY_MSG => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT => restore_main_window(),
                    WM_RBUTTONUP => show_tray_menu(),
                    _ => {}
                }
                LRESULT(0)
            }
            // 托盘右键菜单命令折回本窗口。
            WM_COMMAND => {
                match (wparam.0 & 0xffff) as usize {
                    CMD_RESTORE => restore_main_window(),
                    CMD_QUIT => super::request_quit(),
                    _ => {}
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 恢复主窗口到前台（左键单击 / 双击 / 菜单「恢复主窗口」）。
    fn restore_main_window() {
        let h = super::MAIN_HWND.load(Ordering::SeqCst);
        if h == 0 {
            return;
        }
        let wnd = HWND(h as *mut core::ffi::c_void);
        unsafe {
            let _ = ShowWindow(wnd, SW_RESTORE);
            let _ = SetForegroundWindow(wnd);
        }
    }

    /// 在光标处弹出托盘右键菜单（恢复 / 退出）。
    fn show_tray_menu() {
        if HIDDEN_HWND.load(Ordering::SeqCst) == 0 || TRAY_MENU.load(Ordering::SeqCst) == 0 {
            return;
        }
        let hidden = hidden_hwnd();
        unsafe {
            // TrackPopupMenu 前 SetForegroundWindow：保证菜单能被 Esc/点击外正常收走。
            let _ = SetForegroundWindow(hidden);
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_ok() {
                let _ = TrackPopupMenu(
                    HMENU(TRAY_MENU.load(Ordering::SeqCst) as *mut core::ffi::c_void),
                    TPM_LEFTALIGN | TPM_RIGHTBUTTON,
                    pt.x,
                    pt.y,
                    None,
                    hidden,
                    None,
                );
                // 托盘菜单规范姿势：弹出后向拥有窗口补投一条 WM_NULL（`TrackPopupMenu`
                // 官方示例约定），保证在菜单外点击能被系统正确收走菜单。
                let _ = PostMessageW(Some(hidden), WM_NULL, WPARAM(0), LPARAM(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 托盘测试串行化锁：四个测试共享模块级原子静态（`CLOSE_TO_TRAY`/`MAIN_HWND`/
    /// `FORCE_QUIT`），默认并行跑会互相踩（一个测试 set 后、断言前被另一个测试改掉）→
    /// 每个测试持锁运行，保证确定性（呼应 K7 并行污染纪律）。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 每个测试前重置全部托盘状态（原子无跨测试泄漏）。
    fn reset() {
        set_main_hwnd(0);
        CLOSE_TO_TRAY.store(false, Ordering::SeqCst);
        FORCE_QUIT.store(false, Ordering::SeqCst);
    }

    #[test]
    fn default_off_never_to_tray() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_main_hwnd(12345);
        set_close_to_tray(false);
        assert!(
            !should_close_to_tray(),
            "默认关 → 点 × 应真退出（不隐藏到托盘）"
        );
    }

    #[test]
    fn on_with_known_hwnd_hides_to_tray() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_main_hwnd(12345);
        set_close_to_tray(true);
        assert!(
            should_close_to_tray(),
            "开关开且主窗口已知 → 点 × 应隐藏到托盘"
        );
    }

    #[test]
    fn on_with_unknown_hwnd_never_hides() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_close_to_tray(true);
        set_main_hwnd(0);
        assert!(
            !should_close_to_tray(),
            "主窗口未知（0）→ 恒 false（不隐藏、放行真退出）"
        );
    }

    #[test]
    fn force_quit_consumed_one_shot() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_main_hwnd(12345);
        set_close_to_tray(true);
        // 托盘「退出」→ 强制真退出标记置位。
        request_quit();
        assert!(
            !should_close_to_tray(),
            "FORCE_QUIT 已置位 → 放行真正关闭（false）"
        );
        // 该位已被消费 → 下一次按开关状态判定（回到 true）。
        assert!(
            should_close_to_tray(),
            "FORCE_QUIT 消费后回到按 CLOSE_TO_TRAY 判定"
        );
    }

    /// Windows 专验：内嵌 ICO 经 image 解码 + CreateIconIndirect 能构造**有效非空 HICON**
    /// （无窗口、无托盘图标，仅建一个 GDI 句柄验证机制；[`crate::shutdown`] 释放）。
    /// 真机「任务栏/托盘无图标」的第一道证据：这里绿则 HICON 构造链路没问题。
    #[cfg(windows)]
    #[test]
    fn app_icon_hicon_loads_valid() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        let h = crate::imp::ensure_window_icon();
        assert!(h.is_ok(), "ICO→HICON 构造应成功");
        let raw = h.unwrap();
        assert_ne!(raw, 0, "HICON 不应为空句柄");
        // 幂等：重复调用返回同一句柄（不重复建/泄漏）。
        assert_eq!(crate::imp::ensure_window_icon().unwrap(), raw);
        // 释放（无隐藏窗口时 remove_tray_icon/DestroyWindow 幂等 no-op，DestroyIcon 生效）。
        crate::shutdown();
    }
}
