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

/// 主窗口 HWND（以 `isize` 承载，0 = 未知；由 shell 在窗口建立后写入）。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
/// 「关闭到系统托盘」开关（文件菜单切换，随 `UserPrefs` 立即持久化；默认关）。
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);
/// 「强制真退出」一次性标记：托盘菜单「退出」置位后经 `WM_CLOSE` 关闭主窗口；
/// [`should_close_to_tray`] 读到该位返回 false 并**消费**它（下一次按开关状态判定）。
static FORCE_QUIT: AtomicBool = AtomicBool::new(false);

/// 记录主窗口 HWND（`isize` 承载的 HWND；0 = 未知）。线程安全。
pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, Ordering::SeqCst);
}

/// 翻转「关闭到系统托盘」开关：`enabled` → 创建托盘图标（幂等，已建则跳过）；
/// 关闭 → 移除托盘图标（幂等）。返回创建/移除是否成功（失败不 panic）。
/// 测试构建只翻录 flag，**不真建托盘图标**。
pub fn set_close_to_tray(enabled: bool) -> bool {
    CLOSE_TO_TRAY.store(enabled, Ordering::SeqCst);
    #[cfg(windows)]
    {
        if cfg!(test) {
            return true;
        }
        if enabled {
            imp::add_tray_icon()
        } else {
            imp::remove_tray_icon()
        }
    }
    #[cfg(not(windows))]
    {
        true
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
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
        NIM_DELETE, NIN_SELECT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DestroyWindow, GetCursorPos, LoadIconW, RegisterClassW, SetForegroundWindow,
        ShowWindow, TrackPopupMenu, HCURSOR, HICON, HMENU, IDI_APPLICATION, MF_STRING,
        SW_RESTORE, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE,
        WNDCLASSW, WNDCLASS_STYLES, WM_APP, WM_COMMAND, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_RBUTTONUP,
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

    /// 创建托盘图标（`NIM_ADD`，幂等：已建隐藏窗口则重复创建会失败，静默返回 false）。
    pub(super) fn add_tray_icon() -> bool {
        let Ok(hwnd) = ensure_hidden_window() else {
            return false;
        };
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) };
        let Ok(hicon) = icon else {
            return false;
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

    /// 优雅清理：移除图标 + 销毁隐藏窗口 + 销毁菜单（幂等）。
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
        assert!(set_close_to_tray(true), "开启开关应成功（测试构建不真建图标）");
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
}
