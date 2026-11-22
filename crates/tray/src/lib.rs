//! 系统托盘 crate：把「关闭到系统托盘」相关的 unsafe Win32 调用**全部收敛在本 crate**，
//! 对外暴露**线程安全**的纯函数式 API（内部全部模块级 static / 原子，无裸 `static mut`）。
//!
//! 行为（Windows）：
//! - 注册一个**隐藏消息窗口**（按 UI 线程创建、从不 `ShowWindow`）；托盘回调消息经
//!   gpui 主循环既有 `GetMessageW`/`DispatchMessageW` 派发到该窗口的 `WndProc`；
//! - `Shell_NotifyIconW` 创建/移除托盘图标（幂等）：左键（单击/双击）恢复主窗口置前、
//!   右键 `TrackPopupMenu` 弹「恢复主窗口 / 退出」菜单；
//! - `should_close_to_tray()` 供主窗口 `on_window_should_close` 回调判定：开关开、主窗口
//!   已知、**托盘图标确实已挂载**、且无「强制真退出」标记 → 隐藏到托盘（返回 false 拦下
//!   关闭）；否则放行（图标未挂载时放行真关窗，避免「窗口消失 + 托盘无入口」，P1-C8）。
//! - 隐藏窗口 wndproc 监听 `TaskbarCreated`（Explorer 重启广播）→ 重挂托盘图标，防
//!   Explorer 崩溃/重启后图标永失。
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
/// 托盘图标是否**确实已挂载**（`add_tray_icon` 的 `NIM_ADD` 成功才置位；`remove_tray_icon`/
/// `shutdown`/Explorer 重启重挂失败时清位）。`should_close_to_tray` 要求置位才允许隐藏——
/// 防止「窗口消失 + 托盘无入口」（P1-C8）。
static TRAY_ICON_REGISTERED: AtomicBool = AtomicBool::new(false);

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

/// 测试辅助（跨 crate 集成测试模拟「托盘图标已挂载」态，见 `shell/tests/tray_menu.rs`）。
/// 生产路径的挂载位由实现侧 `add_tray_icon` 成功后置位、`remove_tray_icon`/`shutdown`/
/// Explorer 重启重挂时清位，应用代码不应手动调用。
#[doc(hidden)]
pub fn set_tray_icon_registered(registered: bool) {
    TRAY_ICON_REGISTERED.store(registered, Ordering::SeqCst);
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
/// 开关开 **且** 主窗口已知 **且** 托盘图标已挂载 **且** 无强制退出标记 → true；否则 false。
/// 图标未挂载时**拒绝**隐藏（放行真关窗），避免「窗口消失 + 托盘无入口」（P1-C8）。
/// 读取时**消费** `FORCE_QUIT` 一次性标记（托盘「退出」后放行一次真正关闭）。
pub fn should_close_to_tray() -> bool {
    if !CLOSE_TO_TRAY.load(Ordering::SeqCst) {
        return false;
    }
    if MAIN_HWND.load(Ordering::SeqCst) == 0 {
        return false;
    }
    if !TRAY_ICON_REGISTERED.load(Ordering::SeqCst) {
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

/// 当前操作系统完整版本号（「帮助 → 关于」信息行同款）：Windows →
/// `Windows NT {major}.{minor}.{build}`（经 `RtlGetVersion`，取**真实**版本、
/// 不随兼容层/清单虚报）；取数失败回退 `Windows NT unknown`；非 `windows` 目标
/// 回退 `Other OS`（对齐 tray 既有 target 门控姿势，不破坏跨平台构建）。
/// unsafe Win32 调用收敛在本 crate。
pub fn os_version() -> String {
    #[cfg(windows)]
    {
        imp::os_version_impl()
    }
    #[cfg(not(windows))]
    {
        String::from("Other OS")
    }
}

/// 优雅清理（`on_app_quit` 兜底钩子调用，弱要求）：移除托盘图标 + 销毁隐藏窗口
/// （进程退出系统也会自动清托盘图标，此为 best-effort）。
pub fn shutdown() {
    #[cfg(windows)]
    imp::shutdown();
}

// -------------------------------------------------------------------------
// 剪贴板文件操作（CF_HDROP + Preferred DropEffect）+ 资源管理器定位
// -------------------------------------------------------------------------

/// 剪贴板文件操作的拖动效果（资源管理器语义：粘贴 = 复制 / 移动）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropEffect {
    /// 资源管理器「复制」（粘贴 → 复制文件）。
    Copy,
    /// 资源管理器「剪切」（粘贴 → 移动文件）。
    Move,
}

/// 把单条**绝对路径**作为「文件操作」写入系统剪贴板（`CF_HDROP` + `Preferred DropEffect`，
/// `copy=true` → effect=1 复制 / `false` → effect=2 移动）。语义 = Windows 资源管理器
/// 「复制/剪切」，粘贴到 Explorer / 其它能消费文件操作的应用即复制/移动。**单路径**。
/// 剪贴板被其它进程占用（`OpenClipboard` 失败）等 → `Err`（调用方状态栏提示，不 panic）。
pub fn set_clipboard_file_drop(
    path: &std::path::Path,
    copy: bool,
) -> Result<(), &'static str> {
    #[cfg(windows)]
    {
        imp::set_clipboard_file_drop_impl(path, copy)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, copy);
        Err("该平台不支持剪贴板文件操作")
    }
}

/// 读回当前系统剪贴板的文件操作（`CF_HDROP` 解析）：返回 `(绝对路径列表, 拖动效果)`。
/// 剪贴板无该格式 / 被占用 / 解析失败 → `None`。**测试对拍与后续扩展用**（真实「粘贴」
/// 由资源管理器等消费方执行，本函数只回读）。
pub fn read_clipboard_file_drop() -> Option<(Vec<std::path::PathBuf>, DropEffect)> {
    #[cfg(windows)]
    {
        imp::read_clipboard_file_drop_impl()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 把 **紧凑 BGRA** 像素写为系统剪贴板图片（`CF_DIB`，32bpp BI_RGB、`biHeight` 负数
/// 顶朝上）。语义 = 任意图片查看器/编辑器「粘贴 / 插入图片」直接可用。**像素字节即
/// DIB 像素区**（`w × h × 4`，无 stride），blob = `BITMAPINFOHEADER(40B) + 像素`。
/// 剪贴板被其它进程占用 / 内存不足 → `Err`（调用方提示，不 panic）。
/// 非 `windows` 目标编译为 no-op 错误（不破坏跨平台）。
pub fn set_clipboard_image_bgra(bgra: &[u8], w: u32, h: u32) -> Result<(), &'static str> {
    #[cfg(windows)]
    {
        imp::set_clipboard_image_bgra_impl(bgra, w, h)
    }
    #[cfg(not(windows))]
    {
        let _ = (bgra, w, h);
        Err("该平台不支持剪贴板图片")
    }
}

/// 读回当前系统剪贴板的 `CF_DIB` 图片：返回 `(BGRA 像素, 宽, 高)`（32bpp 归一；
/// 底朝上 DIB 自动翻转为顶朝上、与 [`set_clipboard_image_bgra`] 写入布局一致，便于
/// 逐字节对拍）。剪贴板无该格式 / 非 32bpp / 被占用 → `None`。**测试对拍与粘贴扩展用**。
pub fn read_clipboard_image_bgra() -> Option<(Vec<u8>, u32, u32)> {
    #[cfg(windows)]
    {
        imp::read_clipboard_image_bgra_impl()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 清空系统剪贴板当前内容（`EmptyClipboard`）。**测试夹具用**：清掉可能残留的旧 DIB /
/// 文本（Windows 剪贴板跨进程持久，上一进程/上一轮写入会留在系统里），让「写剪贴板 →
/// 读回」对拍只读到本次写入，杜绝读到陈旧数据。清空失败（被占用/无桌面）→ 静默 no-op，
/// 不 panic（调用方语义 = 「尽力清空」）。非 `windows` 目标为 no-op。
pub fn clear_clipboard() {
    #[cfg(windows)]
    imp::clear_clipboard_impl();
    #[cfg(not(windows))]
    {}
}

/// 构造「在文件管理器中显示」定位命令（**只构造不 spawn**；spawn 由 UI 动作路径负责并在
/// 失败时提示/静默）。平台定制（Windows 行为逐字节不变）：
/// - Windows：`explorer /select,<绝对路径>`（选中该文件/目录的资源管理器窗口，单参数标准姿势）；
/// - macOS：`open -R <绝对路径>`（Finder 中选中该文件/目录）；
/// - Linux/其它 Unix：`xdg-open <所在目录>`（打开所在目录；系统缺 `xdg-open` 时 spawn
///   失败由消费方状态栏提示，符合既有容错面——不在此处预检）；
/// - 未知平台：退回 Windows 姿势（仅构造，失败于 spawn 阶段）。
pub fn reveal_in_explorer_command(path: &std::path::Path) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(format!("/select,{}", path.to_string_lossy()));
        cmd
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg("-R").arg(path);
        cmd
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(path);
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(dir);
        cmd
    }
    #[cfg(not(any(windows, target_os = "macos", unix)))]
    {
        let mut cmd = std::process::Command::new("explorer");
        cmd.arg(format!("/select,{}", path.to_string_lossy()));
        cmd
    }
}

/// DROPFILES 头（`pFiles = sizeof(DROPFILES) = 20`、`pt = 0`、`fNC = false`、
/// `fWide = true` —— UTF-16LE 路径）后接 UTF-16 绝对路径（`\0` 分隔）+ 结尾双 `\0`，
/// 正是 `SetClipboardData(CF_HDROP, …)` 的全局内存布局。纯函数，可单测字节级对拍。
/// 仅 Windows 使用（DROPFILES/POINT/BOOL 布局取自 windows crate，target 门控依赖）——
/// 非 Windows 目标不编译（`mod imp` 剪贴板实现同为 cfg(windows)，无其它调用方）。
#[cfg(windows)]
pub(crate) fn build_dropfiles_bytes(paths: &[&str]) -> Vec<u8> {
    let header_len = size_of::<windows::Win32::UI::Shell::DROPFILES>();
    let mut out: Vec<u8> = Vec::with_capacity(
        header_len
            + paths.iter().map(|p| p.encode_utf16().count() * 2).sum::<usize>()
            + 2,
    );
    // 头 20 字节：pFiles=20 / pt=0 / fNC=0 / fWide=1（结构原生布局，x86/x64 均 20 字节）。
    {
        let h = windows::Win32::UI::Shell::DROPFILES {
            pFiles: header_len as u32,
            pt: windows::Win32::Foundation::POINT::default(),
            fNC: windows::core::BOOL(0),
            fWide: windows::core::BOOL(1),
        };
        let hbytes = unsafe {
            std::slice::from_raw_parts(
                (&h as *const windows::Win32::UI::Shell::DROPFILES).cast::<u8>(),
                header_len,
            )
        };
        out.extend_from_slice(hbytes);
    }
    for p in paths {
        for u in p.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    // 结尾双 `\0`：上面每个路径末尾已有一个 `\0`，列表末尾再补一个凑成双 `\0`。
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// 解析 CF_HDROP 全局内存字节 → 绝对路径列表（UTF-16；跳过空项）。纯函数，可单测。
/// 文件列表从头部 `pFiles` 字段给出的偏移开始（`DROPFILES` 头 20 字节被跳过）。
pub(crate) fn parse_dropfiles_bytes(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if bytes.len() < 4 {
        return out;
    }
    // 头 4 字节 = pFiles：文件列表起始偏移（越界防御钳到缓冲末尾）。
    let start = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let start = start.min(bytes.len());
    let mut i = start;
    while i + 1 < bytes.len() {
        let mut w = [0u16; 1];
        w[0] = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if w[0] == 0 {
            break;
        }
        let mut units: Vec<u16> = Vec::new();
        while w[0] != 0 {
            units.push(w[0]);
            i += 2;
            if i + 1 >= bytes.len() {
                break;
            }
            w[0] = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        }
        if !units.is_empty()
            && let Ok(s) = String::from_utf16(&units) {
                out.push(s);
            }
        i += 2; // 跳过本路径结尾 `\0`（若未触及缓冲末尾）
    }
    out
}

/// 解析 CF_DIB blob → `(BGRA 像素[顶朝上], 宽, 高)`。仅支持 **32bpp BI_RGB**（本 crate
/// 写入与「粘贴图片」的通用形态）；`biHeight` 为正（底朝上）时自动翻转为顶朝上。
/// 头 < 40B / 尺寸越界 / 非 32bpp → `None`。纯函数，可单测字节级对拍。
pub(crate) fn parse_dib_bytes(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if bytes.len() < 40 {
        return None;
    }
    let bi_size = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    const HDR_MIN: usize = 40;
    if bi_size < HDR_MIN || bytes.len() < bi_size {
        return None;
    }
    let hdr = &bytes[..bi_size];
    let width = i32::from_le_bytes(hdr[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(hdr[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(hdr[12..14].try_into().ok()?);
    let bit_count = u16::from_le_bytes(hdr[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(hdr[16..20].try_into().ok()?);
    if width <= 0 || height == 0 || planes != 1 || bit_count != 32 || compression != 0 {
        return None;
    }
    let bottom_up = height > 0;
    let (w, h) = (width as u32, height.unsigned_abs());
    let row_bytes = (w as usize) * 4;
    // 像素区从 `biSize` 偏移开始；32bpp 行天然 4 字节对齐（DIB 无对齐填充）。
    let data = &bytes[bi_size..];
    if data.len() < row_bytes * h as usize {
        return None;
    }
    let mut bgra = Vec::with_capacity(row_bytes * h as usize);
    for row in 0..h as usize {
        let src_row = if bottom_up { h as usize - 1 - row } else { row };
        let src = &data[src_row * row_bytes..(src_row + 1) * row_bytes];
        bgra.extend_from_slice(&src[..row_bytes]);
    }
    Some((bgra, w, h))
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
    /// `RegisterWindowMessageW("TaskbarCreated")` 的系统广播消息号（0 = 未注册/注册失败）。
    /// Explorer 崩溃/重启后向全部顶层窗口广播该消息；隐藏窗口收到后重挂托盘图标。
    static TASKBAR_CREATED_MSG: AtomicIsize = AtomicIsize::new(0);

    fn hidden_hwnd() -> HWND {
        HWND(HIDDEN_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
    }

    /// 注册（幂等）并返回 `TaskbarCreated` 消息号（Explorer 重启广播；失败返回 0 不 panic）。
    /// UI 线程调用（经 [`ensure_hidden_window`]）；进程内重复注册返回同一 id，可缓存。
    fn taskbar_created_msg() -> u32 {
        use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
        let cached = TASKBAR_CREATED_MSG.load(Ordering::SeqCst);
        if cached != 0 {
            return cached as u32;
        }
        // SAFETY：`RegisterWindowMessageW` 把「TaskbarCreated」注册进系统级 0xC000-0xFFFF
        // 消息区间，Explorer 重启时向顶层窗口广播；进程内幂等（重复注册返回同一 id）。
        let id = unsafe { RegisterWindowMessageW(PCWSTR::from_raw(w!("TaskbarCreated").as_ptr())) };
        if id == 0 {
            return 0;
        }
        TASKBAR_CREATED_MSG.store(id as isize, Ordering::SeqCst);
        id
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
        // 注册 Explorer 重启广播（TaskbarCreated）：隐藏窗口 wndproc 收到后重挂托盘图标。
        let _ = taskbar_created_msg();
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
        let img = kimg::ImageReader::new(std::io::Cursor::new(crate::APP_ICO))
            .with_guessed_format()
            .map_err(|_| ())?
            .decode()
            .map_err(|_| ())?;
        // 288×288 原图缩到 64×64 构造 HICON（托盘/任务栏 ~16-48px 显示，系统缩放；
        // 更大的 HICON 在 W32 图标路径缩放质量/兼容性更差且 GDI 位图更大）。
        let rgba = img.to_rgba8();
        let small =
            kimg::imageops::resize(&rgba, 64, 64, kimg::imageops::FilterType::Lanczos3);
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
            let mask_len = ((w as usize).div_ceil(32) * 4) * (h as usize);
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

    /// `RtlGetVersion` 取操作系统真实 major/minor/build 版本号（不经兼容层/清单虚报；
    /// Windows 7/8/10/11 均返回实际内核版本）。返回 `Windows NT {maj}.{min}.{build}`；
    /// 返回码非成功 → 回退 `Windows NT unknown`，不 panic。单 UI 线程调用。
    pub(super) fn os_version_impl() -> String {
        use windows::Wdk::System::SystemServices::RtlGetVersion;
        use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
        // SAFETY：`RtlGetVersion` 按 `dwOSVersionInfoSize` 指明大小向调用方缓冲写入
        // OSVERSIONINFOW（未初始化的剩余字段由调用方填充/系统覆盖）；`dwOSVersionInfoSize`
        // 必须初始化为结构大小（Windows 校验），再以 `is_ok()` 判返回码。单 UI 线程、无并发。
        let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        let status = unsafe { RtlGetVersion(&mut info) };
        if status.is_ok() {
            format!(
                "Windows NT {}.{}.{}",
                info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
            )
        } else {
            String::from("Windows NT unknown")
        }
    }

    /// 创建托盘图标（`NIM_ADD`，幂等：已建隐藏窗口则重复创建会失败，静默返回 false）。
    /// 成功时置位 [`super::TRAY_ICON_REGISTERED`]（允许隐藏到托盘）；`NIM_ADD` 失败**不清位**
    /// ——失败可能是「图标已存在」（幂等重入/恢复流程）或 Explorer 未就绪，此时保留既有
    /// 挂载判定，清位只发生在 `remove_tray_icon`/`shutdown`/Explorer 重启重挂路径。
    pub(super) fn add_tray_icon() -> bool {
        let Ok(hwnd) = ensure_hidden_window() else {
            super::TRAY_ICON_REGISTERED.store(false, Ordering::SeqCst);
            return false;
        };
        // 优先用程序图标（内嵌 ICO 变色龙）；创建失败回退系统默认图标（保底，不 panic）。
        let hicon: HICON = match ensure_window_icon() {
            Ok(raw) => HICON(raw as *mut core::ffi::c_void),
            Err(()) => {
                let Ok(ic) = (unsafe { LoadIconW(None, IDI_APPLICATION) }) else {
                    super::TRAY_ICON_REGISTERED.store(false, Ordering::SeqCst);
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
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data).as_bool() };
        if added {
            super::TRAY_ICON_REGISTERED.store(true, Ordering::SeqCst);
        }
        added
    }

    /// 移除托盘图标（`NIM_DELETE`，幂等：从未创建也返回 true）。同时清挂载位。
    pub(super) fn remove_tray_icon() -> bool {
        super::TRAY_ICON_REGISTERED.store(false, Ordering::SeqCst);
        if HIDDEN_HWND.load(Ordering::SeqCst) == 0 {
            return true;
        }
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hidden_hwnd();
        data.uID = TRAY_ICON_ID;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data).as_bool() }
    }

    /// Explorer 重启（隐藏窗口收到 `TaskbarCreated`）→ 旧图标随旧任务栏销毁：先清挂载位
    /// （当前任务栏上确实没有该图标），再 `NIM_ADD` 重挂；重挂失败（Explorer 尚未就绪）→
    /// 保持「未挂载」语义（拒绝隐藏到托盘，点 × 真关窗）。
    pub(super) fn rerack_tray_icon_after_taskbar_restart() {
        super::TRAY_ICON_REGISTERED.store(false, Ordering::SeqCst);
        add_tray_icon();
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
        let _ = remove_tray_icon(); // 同时清 TRAY_ICON_REGISTERED
        super::MAIN_HWND.store(0, Ordering::SeqCst);
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

    /// 写剪贴板文件操作（CF_HDROP + Preferred DropEffect）。unsafe Win32 全收敛于此。
    /// 单 UI 线程调用（gpui 主线程菜单回调）；失败返回 Err（剪贴板占用 / 内存 / 格式）
    /// 不 panic，调用方状态栏提示。句柄管理：失败路径 `GlobalFree` 归还、成功路径
    /// 剪贴板接管（SetClipboardData 后不 free）。
    pub(super) fn set_clipboard_file_drop_impl(
        path: &std::path::Path,
        copy: bool,
    ) -> Result<(), &'static str> {
        use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
            SetClipboardData,
        };
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS, GMEM_MOVEABLE, GMEM_ZEROINIT};
        use windows::Win32::System::Ole::CF_HDROP;
        use windows::core::{w, PCWSTR};

        let abs = path.to_string_lossy();
        let bytes = super::build_dropfiles_bytes(&[&abs]);
        let gflags = GLOBAL_ALLOC_FLAGS(GMEM_MOVEABLE.0 | GMEM_ZEROINIT.0);
        // SAFETY：系统剪贴板标准协议（OpenClipboard → EmptyClipboard → SetClipboardData，
        // 全局内存经 GlobalAlloc/Lock/Copy/Unlock）。Windows 上单 UI 线程串行调用，无并发。
        unsafe {
            if OpenClipboard(None).is_err() {
                return Err("无法访问系统剪贴板（可能被其它程序占用）");
            }
            let result = (|| {
                EmptyClipboard().ok();
                // CF_HDROP 块：DROPFILES 布局字节 → 移交剪贴板。
                let hdrop = match GlobalAlloc(gflags, bytes.len()) {
                    Ok(h) => h,
                    Err(_) => return Err("系统内存不足"),
                };
                let p = GlobalLock(hdrop);
                if p.is_null() {
                    let _ = GlobalFree(Some(hdrop));
                    return Err("系统内存不足");
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
                GlobalUnlock(hdrop).ok();
                if SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hdrop.0))).is_err() {
                    let _ = GlobalFree(Some(hdrop));
                    return Err("写入剪贴板失败");
                }
                // Preferred DropEffect 块：DWORD 1=COPY / 2=MOVE（资源管理器「复制/剪切」）。
                let fmt = RegisterClipboardFormatW(PCWSTR::from_raw(
                    w!("Preferred DropEffect").as_ptr(),
                ));
                if fmt != 0 {
                    let effect: u32 = if copy { 1 } else { 2 };
                    let heffect = GlobalAlloc(gflags, std::mem::size_of::<u32>()).unwrap_or(HGLOBAL(std::ptr::null_mut()));
                    if !heffect.0.is_null() {
                        let p2 = GlobalLock(heffect);
                        if !p2.is_null() {
                            std::ptr::copy_nonoverlapping(
                                (&effect as *const u32).cast::<u8>(),
                                p2.cast::<u8>(),
                                4,
                            );
                            GlobalUnlock(heffect).ok();
                            if SetClipboardData(fmt, Some(HANDLE(heffect.0))).is_err() {
                                let _ = GlobalFree(Some(heffect));
                            }
                        } else {
                            let _ = GlobalFree(Some(heffect));
                        }
                    }
                }
                Ok(())
            })();
            CloseClipboard().ok();
            result
        }
    }

    /// 读回剪贴板文件操作（CF_HDROP + Preferred DropEffect）。同一 unsafe 家庭成员；
    /// 供测试对拍与后续扩展（真实「粘贴」由资源管理器等消费方执行）。
    pub(super) fn read_clipboard_file_drop_impl(
    ) -> Option<(Vec<std::path::PathBuf>, super::DropEffect)> {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use windows::Win32::System::Ole::CF_HDROP;
        use windows::core::{w, PCWSTR};

        // SAFETY：OpenClipboard 期间 GetClipboardData 返回句柄、GlobalLock 映射为字节视图、
        // 用后 Unlock + CloseClipboard（读不改全局态）。
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                let hdrop = GetClipboardData(CF_HDROP.0 as u32).ok()?;
                if hdrop.0.is_null() {
                    return None;
                }
                // 剪贴板返回 HANDLE；以下全局内存访问一律视为 HGLOBAL（同一内核对象句柄）。
                let hg = HGLOBAL(hdrop.0);
                let size = GlobalSize(hg);
                let p = GlobalLock(hg);
                if p.is_null() {
                    return None;
                }
                let mut bytes = vec![0u8; size];
                std::ptr::copy_nonoverlapping(p.cast::<u8>(), bytes.as_mut_ptr(), size);
                GlobalUnlock(hg).ok();
                if bytes.is_empty() {
                    return None;
                }
                let paths: Vec<std::path::PathBuf> = super::parse_dropfiles_bytes(&bytes)
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect();
                // 拖动效果回读：DWORD 2=Move，其余按 Copy。
                let mut effect = super::DropEffect::Copy;
                let fmt = RegisterClipboardFormatW(PCWSTR::from_raw(w!("Preferred DropEffect").as_ptr()));
                if fmt != 0 {
                    let hfx = GetClipboardData(fmt).ok()?;
                    // 恒真死守卫（dormancy 休眠保留，K24）：`GetClipboardData` 失败返回
                    // Err 已被 `?` 提前短路，成功句柄必非空 → `hfx.0.is_null()` 恒不成立；
                    // 保留作防御式检查，不删。
                    if !hfx.0.is_null() {
                        let hg2 = HGLOBAL(hfx.0);
                        let px = GlobalLock(hg2);
                        if !px.is_null() {
                            let mut v = 0u32;
                            std::ptr::copy_nonoverlapping(px.cast::<u8>(), (&mut v as *mut u32).cast::<u8>(), 4);
                            GlobalUnlock(hg2).ok();
                            if v == 2 {
                                effect = super::DropEffect::Move;
                            }
                        }
                    }
                }
                Some((paths, effect))
            })();
            CloseClipboard().ok();
            result
        }
    }

    /// 写剪贴板 DIB 图片（CF_DIB，32bpp BI_RGB，`biHeight` 负数 **顶朝上**）。
    /// 同一 unsafe 家庭成员（OpenClipboard → Empty → GlobalAlloc → SetClipboardData → Close；
    /// 失败路径 `GlobalFree` 归还、成功路径剪贴板接管）。单 UI 线程调用。
    /// blob 布局 = `BITMAPINFOHEADER(40B) + w*h*4 BGRA 像素`（紧凑、无 stride），
    /// 正是系统/Office/画图「粘贴图片」消费的 CF_DIB 标准形态。
    pub(super) fn set_clipboard_image_bgra_impl(
        bgra: &[u8],
        w: u32,
        h: u32,
    ) -> Result<(), &'static str> {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::Graphics::Gdi::{BITMAPINFOHEADER, BI_RGB};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS, GMEM_MOVEABLE, GMEM_ZEROINIT};
        use windows::Win32::System::Ole::CF_DIB;

        let expected = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if w == 0 || h == 0 || bgra.len() != expected {
            return Err("无效图片尺寸或像素缓冲");
        }
        let header_bytes = std::mem::size_of::<BITMAPINFOHEADER>();
        let blob_len = header_bytes + expected;
        let gflags = GLOBAL_ALLOC_FLAGS(GMEM_MOVEABLE.0 | GMEM_ZEROINIT.0);
        // SAFETY：系统剪贴板标准协议（OpenClipboard → EmptyClipboard → SetClipboardData，
        // 全局内存经 GlobalAlloc/Lock/Copy/Unlock）。Windows 上单 UI 线程串行调用，无并发。
        unsafe {
            if OpenClipboard(None).is_err() {
                return Err("无法访问系统剪贴板（可能被其它程序占用）");
            }
            let result = (|| {
                EmptyClipboard().ok();
                let hmem = match GlobalAlloc(gflags, blob_len) {
                    Ok(h) => h,
                    Err(_) => return Err("系统内存不足"),
                };
                let p = GlobalLock(hmem);
                if p.is_null() {
                    let _ = GlobalFree(Some(hmem));
                    return Err("系统内存不足");
                }
                let p_bytes = p.cast::<u8>();
                // BITMAPINFOHEADER：biHeight 取负 → 顶朝上（row0 = 图片顶行），像素区即
                // 调用方传来的 BGRA（pdfium 位图本就是顶朝上布局，与 DIB 负高约定天然一致）。
                let hdr: BITMAPINFOHEADER = std::mem::zeroed(); // 未用字段全部零（biClrUsed=0）
                let hdr = BITMAPINFOHEADER {
                    biSize: header_bytes as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..hdr
                };
                std::ptr::copy_nonoverlapping(
                    (&hdr as *const BITMAPINFOHEADER).cast::<u8>(),
                    p_bytes,
                    header_bytes,
                );
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), p_bytes.add(header_bytes), expected);
                GlobalUnlock(hmem).ok();
                if SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(hmem.0))).is_err() {
                    let _ = GlobalFree(Some(hmem));
                    return Err("写入剪贴板失败");
                }
                Ok(())
            })();
            CloseClipboard().ok();
            result
        }
    }

    /// 清空系统剪贴板当前内容（测试夹具用，见 [`crate::clear_clipboard`]）。
    /// Open → Empty → Close；被占用/无桌面 → 静默 no-op（尽力清空，不 panic）。
    pub(super) fn clear_clipboard_impl() {
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard,
        };
        // SAFETY：Open 期间 Empty + Close 为标准清空协议；失败（被占用）静默返回，
        // 不改动其它状态。单 UI 线程调用（与写剪贴板同契约）。
        unsafe {
            if OpenClipboard(None).is_ok() {
                EmptyClipboard().ok();
                CloseClipboard().ok();
            }
        }
    }

    /// 读回剪贴板 `CF_DIB` 图片为 `(BGRA, w, h)`（32bpp 归一，底朝上自动翻转为顶朝上，
    /// 供测试逐字节对拍与粘贴扩展）。非 32bpp / 无该格式 / 占用 → `None`；不 panic。
    pub(super) fn read_clipboard_image_bgra_impl() -> Option<(Vec<u8>, u32, u32)> {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use windows::Win32::System::Ole::CF_DIB;

        // SAFETY：OpenClipboard 期间 GetClipboardData 返回句柄、GlobalLock 映射为字节视图、
        // 用后 Unlock + CloseClipboard（读不改全局态）。
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                let h = GetClipboardData(CF_DIB.0 as u32).ok()?;
                if h.0.is_null() {
                    return None;
                }
                let hg = HGLOBAL(h.0);
                let size = GlobalSize(hg);
                let p = GlobalLock(hg);
                if p.is_null() {
                    return None;
                }
                let bytes = std::slice::from_raw_parts(p.cast::<u8>(), size);
                let out = super::parse_dib_bytes(bytes);
                GlobalUnlock(hg).ok();
                out
            })();
            CloseClipboard().ok();
            result
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
                match wparam.0 & 0xffff {
                    CMD_RESTORE => restore_main_window(),
                    CMD_QUIT => super::request_quit(),
                    _ => {}
                }
                LRESULT(0)
            }
            // Explorer 崩溃/重启广播（TaskbarCreated）：旧托盘图标随旧任务栏销毁 → 重挂
            // （未注册该消息时 `TASKBAR_CREATED_MSG == 0`，不会误匹配任何系统消息）。
            _ => {
                let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::SeqCst);
                if taskbar_created != 0 && msg == taskbar_created as u32 {
                    rerack_tray_icon_after_taskbar_restart();
                    LRESULT(0)
                } else {
                    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
                }
            }
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
        set_tray_icon_registered(false);
    }

    /// 本机为 Windows → `os_version` 必须返回真实版本串（非空 + `Windows NT ` 前缀约定）。
    /// 防御式：即使换到非 Windows 目标也不崩（前缀 `Other` 兜底，见库函数 cfg 分支）。
    #[test]
    fn os_version_returns_windows_nt_prefix_or_other() {
        let v = crate::os_version();
        assert!(!v.is_empty(), "os_version 不得为空");
        assert!(
            v.starts_with("Windows NT ") || v.starts_with("Other"),
            "os_version 前缀不符：{v}"
        );
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
        set_tray_icon_registered(true); // 测试环境不真挂图标，模拟「已挂载」态（P1-C8）
        assert!(
            should_close_to_tray(),
            "开关开且主窗口已知且图标已挂载 → 点 × 应隐藏到托盘"
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
        set_tray_icon_registered(true); // 模拟「已挂载」态：FORCE_QUIT 消费后才回到 true
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

    /// P1 红测：托盘图标**未挂载**时必须拒绝「关闭到托盘」（否则退化为真关窗）。
    ///
    /// 现状缺陷可观测面：`should_close_to_tray()`（lib.rs:60-68）只查
    /// `CLOSE_TO_TRAY && MAIN_HWND != 0 && !FORCE_QUIT` 三个 flag，**完全不核实
    /// 托盘图标是否真的挂载成功**——`add_tray_icon` 的 `NIM_ADD` 返回 false 被静默吞
    /// （lib.rs:577-600），也没有 `TaskbarCreated` 重挂监听。于是「开关开 + 主窗口已
    /// 知」时**即使没有任何托盘图标**，点 × 也会把窗口从任务栏藏掉（托盘无入口，
    /// 只能杀进程）。
    ///
    /// 修复后语义：「关闭到托盘」只应在图标**确实挂载成功**时才被允许；未挂载时点 ×
    /// 应放行真关窗。本测试断言「图标未挂载 → 隐藏决策必须为 false」；修复前恒
    /// true（只看 MAIN_HWND）→ 红。
    ///
    /// 说明：当前代码尚无可测的「图标是否挂载」入口（决定函数输入面只有上述三 flag）
    /// → 本红测以修复后形态为真值；实现侧需随修复加可测钩子（如 `TRAY_ICON_REGISTERED`
    /// 标志 + `#[cfg(test)]` 置位辅助），否则「图标已挂载→允许隐藏」的正向断言无法表达。
    /// 测试环境从不真实挂载图标（K21：测试构建不真建图标/窗口）→ 此处天然处于
    /// 「未挂载」状态。
    #[test]
    fn unmounted_tray_icon_must_refuse_hide_to_tray() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        set_main_hwnd(12345);
        set_close_to_tray(true);
        assert!(
            !should_close_to_tray(),
            "托盘图标未挂载时不得隐藏到托盘（应允许真关窗，避免「窗口消失 + 托盘无入口」）"
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

    /// 剪贴板文件操作测试串行锁（与托盘 flag 测试同锁共用，避免并行脚踩真实剪贴板）。
    static CLIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// DROPFILES 字节布局：pFiles=20 / pt=0 / fNC=0 / fWide=1 / UTF-16 路径 / 结尾双 `\0`。
    /// Windows 专用（build_dropfiles_bytes 为 target 门控）：非 Windows 测试构建不编译。
    #[cfg(windows)]
    #[test]
    fn dropfiles_bytes_layout_header_and_wide() {
        let b = crate::build_dropfiles_bytes(&[r"D:\a\b.txt"]);
        // 头 20 字节 = DROPFILES：pFiles(20)=20, pt(0..8)=0, fNC=0, fWide=1
        assert_eq!(&b[0..4], &[20u8, 0, 0, 0], "pFiles 应为 sizeof(DROPFILES)=20");
        assert!(
            b[4..12].iter().all(|&x| x == 0),
            "pt 应为零"
        );
        assert!(
            b[12..16].iter().all(|&x| x == 0),
            "fNC 应为 false"
        );
        assert_eq!(&b[16..20], &[1u8, 0, 0, 0], "fWide 应为 true（UTF-16）");
        // UTF-16LE 路径：D:\a\b.txt 各字符低字节在前
        let mut p = 20usize;
        for ch in r"D:\a\b.txt".encode_utf16() {
            assert_eq!(b[p], (ch & 0xff) as u8, "UTF-16 低字节字符@{p}");
            assert_eq!(b[p + 1], (ch >> 8) as u8, "UTF-16 高字节字符@{p}");
            p += 2;
        }
        assert_eq!(
            b[p],
            0,
            "路径后应为 `\\0`（单路径：该 `\\0` 即列表双 `\\0` 的第一半）"
        );
        assert_eq!(
            b[p + 1],
            0,
            "列表末尾双 `\\0` 的第二半（CF_HDROP 规范要求双 null 结尾）"
        );
        assert_eq!(
            b.len(),
            p + 4,
            "整体长度 = 头(20) + 路径 UTF-16 + 路径结尾 `\\0`(2) + 补位 `\\0`(2)=44"
        );
    }

    /// 解析回读纯函数 roundtrip：多路径 UTF-16 → 原样还原。
    /// Windows 专用（build_dropfiles_bytes 为 target 门控）：非 Windows 测试构建不编译。
    #[cfg(windows)]
    #[test]
    fn dropfiles_bytes_parse_roundtrip() {
        let paths = [r"D:\a\b.txt", r"E:\文件夹\x 空格.md", r"C:\single"];
        let bytes = crate::build_dropfiles_bytes(&paths);
        let back = crate::parse_dropfiles_bytes(&bytes);
        assert_eq!(back, paths, "UTF-16 路径 roundtrip 应一致");
    }

    /// 剪贴板写→读回真实对拍（含拖动效果）：若 `OpenClipboard` 失败（无交互会话 /
    /// 剪贴板被占用）→ **优雅 skip**（打印说明并返回，不硬失败，K21）。
    /// 副作用收敛：写盘**前**备份原剪贴板（CF_HDROP + Preferred DropEffect + CF_UNICODETEXT），
    /// 经 RAII guard 在测试结束（含 panic）**后**还原——对拍不覆盖用户真实剪贴板。
    #[cfg(windows)]
    #[test]
    fn clipboard_file_drop_roundtrip_with_effect() {
        let _guard = CLIP_LOCK.lock().unwrap();
        let _restore = ClipRestoreGuard(backup_clipboard());
        let target = std::path::Path::new("D:\\gpui-editor\\src\\main.rs");
        if let Err(e) = crate::set_clipboard_file_drop(target, true) {
            eprintln!("[SKIP] 剪贴板不可用，跳过写入回读对拍：{e}");
            return;
        }
        let (paths, effect) = crate::read_clipboard_file_drop().expect("写入后应能读回");
        assert_eq!(paths, vec![target.to_path_buf()], "CF_HDROP 应原样回读单路径");
        assert_eq!(effect, crate::DropEffect::Copy, "COPY 写入 → 效果应为复制");
        // 剪切：效果翻转为 Move。
        crate::set_clipboard_file_drop(target, false).expect("剪切写入应成功");
        let (paths2, effect2) = crate::read_clipboard_file_drop().expect("剪切后应能读回");
        assert_eq!(paths2, vec![target.to_path_buf()]);
        assert_eq!(effect2, crate::DropEffect::Move, "MOVE 写入 → 效果应为移动");
    }

    /// DIB 字节解析纯函数：32bpp blob 头建 40B BITMAPINFOHEADER + 像素 → 原样还原
    /// （顶朝上原序；底朝上自动翻行）；非 32bpp / 头越界 / 尺寸越界 → `None`。
    #[test]
    fn dib_parse_pure_functions() {
        let w = 2u32;
        let h = 2u32;
        let mk_blob = || -> Vec<u8> {
            let mut blob = vec![0u8; 40 + (w * h * 4) as usize];
            blob[0..4].copy_from_slice(&40u32.to_le_bytes());
            blob[4..8].copy_from_slice(&(w as i32).to_le_bytes());
            blob[8..12].copy_from_slice(&(-(h as i32)).to_le_bytes()); // 顶朝上
            blob[12..14].copy_from_slice(&1u16.to_le_bytes());
            blob[14..16].copy_from_slice(&32u16.to_le_bytes());
            blob[16..20].copy_from_slice(&0u32.to_le_bytes()); // BI_RGB
            blob[40..].copy_from_slice(&[
                // 顶朝上：row0 = px0 红, px1 绿；row1 = px2 蓝, px3 黑（BGRA）
                0, 0, 255, 255,
                0, 255, 0, 255,
                255, 0, 0, 255,
                0, 0, 0, 255,
            ]);
            blob
        };
        let px_top_down: Vec<u8> = vec![
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 0, 0, 0, 255,
        ];
        let (bgra, bw, bh) = crate::parse_dib_bytes(&mk_blob()).expect("顶朝上应解析");
        assert_eq!((bw, bh), (w, h));
        assert_eq!(bgra, px_top_down, "顶朝上 → 原行序返回");

        // 底朝上（biHeight 为正）→ 行翻转后与顶朝上同布局
        let mut bottom_up = mk_blob();
        bottom_up[8..12].copy_from_slice(&(h as i32).to_le_bytes());
        let (bgra2, bw2, bh2) = crate::parse_dib_bytes(&bottom_up).expect("底朝上应解析");
        assert_eq!((bw2, bh2), (w, h));
        let px_bottom_down: Vec<u8> = vec![
            255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255,
        ];
        assert_eq!(bgra2, px_bottom_down, "底朝上应翻转为顶朝上（row0 互为镜像）");

        // 非 32bpp → None
        let mut bad = mk_blob();
        bad[14..16].copy_from_slice(&24u16.to_le_bytes());
        assert!(crate::parse_dib_bytes(&bad).is_none(), "24bpp 应拒绝");
        // 头越界 → None
        assert!(crate::parse_dib_bytes(&mk_blob()[..20]).is_none(), "头不足 40B 应拒绝");
        // 像素区不足 → None
        let mut short = mk_blob();
        short.truncate(40 + 8);
        assert!(crate::parse_dib_bytes(&short).is_none(), "像素区不足应拒绝");
    }

    /// 剪贴板图片写→读回真实对拍（CF_DIB）：32bpp BGRA 像素进出系统剪贴板应**逐字节**
    /// 一致（这是「复制图片」端到端回读与粘贴方消费同一信号源的地基）。`OpenClipboard`
    /// 失败（无交互会话/被占用）→ **优雅 skip**（K21）；写盘前备份原剪贴板、RAII 还原。
    #[cfg(windows)]
    #[test]
    fn clipboard_dib_roundtrip() {
        let _guard = CLIP_LOCK.lock().unwrap();
        let _restore = ClipRestoreGuard(backup_clipboard());
        let (w, h) = (3u32, 2u32);
        let mut bgra = Vec::new();
        for i in 0..(w * h) as u8 {
            bgra.extend_from_slice(&[i, i.wrapping_mul(2), i.wrapping_mul(3) + 1, 255]);
        }
        if let Err(e) = crate::set_clipboard_image_bgra(&bgra, w, h) {
            eprintln!("[SKIP] 剪贴板不可用，跳过 CF_DIB 写读对拍：{e}");
            return;
        }
        let (back, bw, bh) = crate::read_clipboard_image_bgra().expect("写入后应能读回 CF_DIB");
        assert_eq!((bw, bh), (w, h), "CF_DIB 尺寸应一致：{}x{}", bw, bh);
        assert_eq!(back, bgra, "CF_DIB 往返应逐字节一致（顶朝上 BGRA）");
    }

    /// 剪贴板备份：对拍测试写盘前的原始内容（读失败项为 `None`，还原时跳过该格式）。
    #[cfg(windows)]
    struct ClipBackup {
        hdrop: Option<Vec<u8>>,
        effect: Option<u32>,
        text: Option<Vec<u8>>,
    }

    /// 备份当前剪贴板的 CF_HDROP 字节、Preferred DropEffect DWORD、CF_UNICODETEXT 字节。
    /// 一次 `OpenClipboard` 会话内完成；打不开（被占用）→ `None`（测试照跑，尽力而为）。
    #[cfg(windows)]
    fn backup_clipboard() -> Option<ClipBackup> {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW};
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
        use windows::core::{w, PCWSTR};
        fn copy_global(handle: windows::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
            unsafe {
                if handle.0.is_null() {
                    return None;
                }
                let hg = HGLOBAL(handle.0);
                let size = GlobalSize(hg);
                if size == 0 {
                    return None;
                }
                let p = GlobalLock(hg);
                if p.is_null() {
                    return None;
                }
                let mut bytes = vec![0u8; size];
                // SAFETY：GlobalLock 周期内把全局内存映射为 `size` 字节视图（GetClipboardData
                // 句柄在 OpenClipboard 会话内有效）。只读拷贝，用后 Unlock。
                std::ptr::copy_nonoverlapping(p.cast::<u8>(), bytes.as_mut_ptr(), size);
                GlobalUnlock(hg).ok();
                Some(bytes)
            }
        }
        // SAFETY：OpenClipboard 期间经 GetClipboardData 只读回读，用后 CloseClipboard。
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let result = (|| {
                let hdrop = GetClipboardData(CF_HDROP.0 as u32)
                    .ok()
                    .and_then(copy_global);
                let fmt = RegisterClipboardFormatW(PCWSTR::from_raw(w!("Preferred DropEffect").as_ptr()));
                let effect = if fmt == 0 {
                    None
                } else {
                    GetClipboardData(fmt).ok().and_then(|h| {
                        copy_global(h).map(|bytes| {
                            let mut v = 0u32;
                            if bytes.len() >= 4 {
                                v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                            }
                            v
                        })
                    })
                };
                let text = GetClipboardData(CF_UNICODETEXT.0 as u32)
                    .ok()
                    .and_then(copy_global);
                Some(ClipBackup {
                    hdrop,
                    effect,
                    text,
                })
            })();
            CloseClipboard().ok();
            result
        }
    }

    /// 还原剪贴板备份：EmptyClipboard 后按各格式写回。仅尽力而为（某格式写失败静默跳过）。
    #[cfg(windows)]
    fn restore_clipboard(backup: ClipBackup) {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
            SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS, GMEM_MOVEABLE,
            GMEM_ZEROINIT,
        };
        use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
        use windows::core::{w, PCWSTR};
        fn write_global(fmt: u32, bytes: &[u8]) -> bool {
            let gflags = GLOBAL_ALLOC_FLAGS(GMEM_MOVEABLE.0 | GMEM_ZEROINIT.0);
            // SAFETY：标准剪贴板写协议（分配全局内存 → 拷贝 → 移交 SetClipboardData）。
            unsafe {
                let Ok(hg) = GlobalAlloc(gflags, bytes.len()) else {
                    return false;
                };
                let p = GlobalLock(hg);
                if p.is_null() {
                    let _ = GlobalFree(Some(hg));
                    return false;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
                GlobalUnlock(hg).ok();
                if SetClipboardData(fmt, Some(HANDLE(hg.0))).is_err() {
                    // 移交失败：归还全局内存（剪贴板未接管）。
                    let _ = GlobalFree(Some(hg));
                    return false;
                }
                true
            }
        }
        // SAFETY：OpenClipboard → EmptyClipboard → SetClipboardData → CloseClipboard 标准序列。
        unsafe {
            if OpenClipboard(None).is_err() {
                return;
            }
            EmptyClipboard().ok();
            if let Some(bytes) = &backup.hdrop {
                write_global(CF_HDROP.0 as u32, bytes);
            }
            if let Some(v) = backup.effect {
                let fmt = RegisterClipboardFormatW(PCWSTR::from_raw(w!("Preferred DropEffect").as_ptr()));
                if fmt != 0 {
                    write_global(fmt, &v.to_le_bytes());
                }
            }
            if let Some(bytes) = &backup.text {
                write_global(CF_UNICODETEXT.0 as u32, bytes);
            }
            CloseClipboard().ok();
        }
    }

    /// RAII：`ClipBackup` 在 `Drop` 时还原（panic 也会还原）。
    #[cfg(windows)]
    struct ClipRestoreGuard(Option<ClipBackup>);
    #[cfg(windows)]
    impl Drop for ClipRestoreGuard {
        fn drop(&mut self) {
            if let Some(backup) = self.0.take() {
                restore_clipboard(backup);
            }
        }
    }

    /// `explorer /select,<abs>` 命令构造（只构造不 spawn——真 spawn 留给用户动作路径）。
    #[test]
    fn reveal_command_selects_absolute_path() {
        let cmd = crate::reveal_in_explorer_command(std::path::Path::new(r"D:\x y\a.rs"));
        assert_eq!(cmd.get_program(), "explorer");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args.len(), 1, "应恰一个 `/select,<path>` 参数");
        assert_eq!(
            args[0],
            r"/select,D:\x y\a.rs",
            "空格路径应原样进单个参数"
        );
    }
}
