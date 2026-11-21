//! PDF 阅读背景：**画布色**（页面四周壁纸）+ **页面位图显示层纸色重着色**，预设映射 /
//! 应用到位。持久化由宿主 `persist_prefs` 落地（本 crate 只提供模型写 [`set_pdf_bg_model`]）。
//!
//! 语义（brief R4/R5/R6）：一律**只改显示层**——画布色改 viewer `bg`（页面外底色），
//! 纸色重着色由 worker 在栅格化输出上对位图像素就地映射（仅**近白纸面**→纸色，文字/
//! 图片/图形等**内容像素保持原色**，见 `pdf::recolor`），**绝不写回 PDF 文件、不改
//! pdfium 解析**；文本层（选区/搜索）独立叠加、不受影响。
//! `pdf_bg` 为全局偏好（config 单一真值）：`default` = 画布跟随主题 + **无重着色**（直通）；
//! 显式预设/自定义 = 画布固定色 + 对应纸色重着色，主题切换跳过（chrome.rs 守卫）。
//! 自定义画布色经 Windows 原生 `CHOOSECOLORW` 取（rfd 0.17 无 ColorPickerDialog——
//! ColorPicker 行在 rfd 文档表中即为未实现；改用手写 `#[link]` FFI 直调 comdlg32，
//! 零新依赖零新 feature），测试构建 / 非 windows 不弹框（no-op，headless 无法真弹）。
//!
//! 解析 / 重着色 / 自定义色应用逻辑在本 crate（`&AppModel` 读数 + `Entity<PdfViewer>`
//! 应用）；`pick_custom_pdf_bg` 的**宿主 window 部分**（当前窗口 HWND 提取 +
//! set_pdf_bg 落地）留在 shell 薄适配。

use config::user::PdfBgPref;
use gpui::App;
use pdf::{PdfViewer, RecolorParams};
use shell_core::{AppModel, MediaReader};
use ui::content_colors::{
    CANVAS_DARK_INK, CANVAS_PRESET_EYE, CANVAS_PRESET_NIGHT, CANVAS_PRESET_PARCHMENT,
};

/// 解析 `#rrggbb`（去空白/前缀，6 位十六进制）→ `[u8;3]`（R,G,B）。非法 → `None`。
pub fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// RGB 字节三元组 → `Hsla`（与 `gpui::rgb(hex).into()` 同值；画布预设与自定义共用）。
fn rgb_bytes_to_hsla([r, g, b]: [u8; 3]) -> gpui::Hsla {
    gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
}

/// 画布色「深色」判定（感知亮度 < 0.5 → 深色）：深色画布配浅墨迹（`CANVAS_DARK_INK`）保证
/// 文字可读；浅色画布（护眼/羊皮纸/浅色自定义）只染纸面、内容直通。
fn is_dark_bg(rgb: [u8; 3]) -> bool {
    let [r, g, b] = rgb;
    (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0 < 0.5
}

/// 解析 `#rrggbb` → `Hsla`（画布底色，`alpha=1`）。与 config crate 的存储校验同一套
/// hex 语法，double 兜底。
pub fn parse_hex_hsla(s: &str) -> Option<gpui::Hsla> {
    let [r, g, b] = parse_hex_rgb(s)?;
    Some(rgb_bytes_to_hsla([r, g, b]))
}

/// 当前 PDF 画布底色映射（读 `&AppModel`）：`None` = 跟随主题（viewer 回落
/// `default_canvas`）；自定义档自定义色缺失/非法 → 回退 `None`（default，安全护栏）。
pub fn canvas_bg(m: &AppModel) -> Option<gpui::Hsla> {
    match m.pdf_bg {
        PdfBgPref::Default => None,
        PdfBgPref::Night => Some(rgb_bytes_to_hsla(CANVAS_PRESET_NIGHT)),
        PdfBgPref::Eye => Some(rgb_bytes_to_hsla(CANVAS_PRESET_EYE)),
        PdfBgPref::Parchment => Some(rgb_bytes_to_hsla(CANVAS_PRESET_PARCHMENT)),
        PdfBgPref::Custom => m.pdf_bg_custom.as_deref().and_then(parse_hex_hsla),
    }
}

/// 当前**页面位图显示层纸墨重着色**参数（读 `&AppModel`；R4）：`None` = 直通（默认档）。
/// 深色档（夜间/自定义深色）画布同步纸色，并**同时提亮墨迹**（`ink`）：浅纸上的
/// 黑字若只改纸面会变成「黑字黑底 + 浅灰描边」不可读（用户复报），须把近黑墨迹
/// 一并映射为浅色（只动近黑像素，图片/中间调内容不动，见 `pdf::recolor`）；
/// 浅色档（护眼/羊皮纸/浅色自定义）只染纸面、内容直通（既有行为）。
pub fn canvas_recolor(m: &AppModel) -> Option<RecolorParams> {
    let dark_ink = is_dark_bg;
    match m.pdf_bg {
        PdfBgPref::Default => None,
        PdfBgPref::Night => {
            Some(RecolorParams { bg: CANVAS_PRESET_NIGHT, ink: Some(CANVAS_DARK_INK) })
        }
        PdfBgPref::Eye => Some(RecolorParams { bg: CANVAS_PRESET_EYE, ink: None }),
        PdfBgPref::Parchment => Some(RecolorParams { bg: CANVAS_PRESET_PARCHMENT, ink: None }),
        PdfBgPref::Custom => m
            .pdf_bg_custom
            .as_deref()
            .and_then(parse_hex_rgb)
            .map(|bg| RecolorParams { bg, ink: dark_ink(bg).then_some(CANVAS_DARK_INK) }),
    }
}

/// 把当前 `pdf_bg` 偏好（读 `&AppModel`）应用到单个 PDF 阅读器：
/// - 画布：显式色 → `set_background`；default → `clear_background` 回落主题；
/// - 位图显示层：`set_recolor`（纸色重着色——仅近白纸面→纸色，内容像素不动；
///   None = 直通；变更走一次性缓存失效重渲）。
pub fn apply_background(m: &AppModel, pdf: &gpui::Entity<PdfViewer>, cx: &mut App) {
    let bg = canvas_bg(m);
    let recolor = canvas_recolor(m);
    pdf.update(cx, |view, cx| {
        match bg {
            Some(h) => view.set_background(h),
            None => view.clear_background(),
        }
        view.set_recolor(recolor, cx);
        cx.notify();
    });
}

/// 把当前 `pdf_bg` 偏好（读 `&AppModel`）应用到本窗口全部 PDF 阅读器（打开 / 换主题 /
/// 改偏好共用；与 `refresh_theme_ui` 一致的窗口内作用域）。
pub fn apply_backgrounds(m: &AppModel, cx: &mut App) {
    let pdfs: Vec<gpui::Entity<PdfViewer>> = m
        .tabs
        .iter()
        .filter_map(|t| match &t.payload.reader {
            MediaReader::Pdf(pv) => Some(pv.clone()),
            _ => None,
        })
        .collect();
    for pv in pdfs {
        apply_background(m, &pv, cx);
    }
}

/// 模型写：设 PDF 画布背景偏好并立即对全部阅读器应用（右键「背景色」预设/自定义点击
/// 共用）。`custom` 仅在 `Some` 时更新（重选「自定义」保留上次颜色；预设不覆盖存储值）。
/// 持久化（`persist_prefs`）与 `cx.notify` 由宿主在薄适配里落地（本函数是原
/// `EditorApp::set_pdf_bg` 的模型 + 应用部分）。
pub fn set_pdf_bg_model(m: &mut AppModel, pref: PdfBgPref, custom: Option<String>) {
    m.pdf_bg = pref;
    if custom.is_some() {
        m.pdf_bg_custom = custom;
    }
}

/// Windows 原生取色对话框（`CHOOSECOLORW`，comdlg32）：返回选中色 `#rrggbb`；取消 →
/// `None`。初始色为当前画布色（`None` → 白）。`lpCustColors` 用进程级静态缓冲——
/// Windows 会保存用户在本对话框「添加到自定义颜色」里的色值，跨次调用保留（按
/// comdlg32 规范须预先初始化，全填白）。
///
/// 实现走**手写 `#[link]` FFI**（仅此一个结构体 + 一个函数）而非 `windows` crate：拉入
/// `Win32::UI::Controls::Dialogs` 整模块会引入其巨量类型，与 shell 巨型测试二进制叠加后
/// 触发 MSVC 链接 `LNK1140: PDB 类型上限`——手写 FFI 既零新依赖也零 PDB 暴涨。
///
/// 宿主只在 `pick_custom_pdf_bg`
/// 里提取当前窗口 HWND 后调用本函数。
#[cfg(all(windows, not(test)))]
pub fn pick_color(initial: Option<gpui::Hsla>, owner_hwnd: isize) -> Option<String> {
    use std::sync::OnceLock;
    // 自定义色缓冲（16 个「添加到自定义颜色」槽位；跨次调用保留，首启全填白）。
    static CUSTOM: OnceLock<std::sync::Mutex<[u32; 16]>> = OnceLock::new();
    let mut custom = CUSTOM
        .get_or_init(|| std::sync::Mutex::new([0x00ff_ffff; 16]))
        .lock()
        .unwrap();
    let initial_rgba: gpui::Rgba = initial.unwrap_or(gpui::white()).into();
    let to8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    let r = to8(initial_rgba.r);
    let g = to8(initial_rgba.g);
    let b = to8(initial_rgba.b);
    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)] // 与 Win32 `CHOOSECOLORW` 原生名一致
    #[repr(C)]
    struct CHOOSECOLORW {
        l_struct_size: u32,
        hwnd_owner: isize,
        h_instance: isize,
        rgb_result: u32,
        lp_cust_colors: *mut u32,
        flags: u32,
        l_cust_data: isize,
        lpfn_hook: *const (),
        lp_template_name: *const u16,
    }

    const CC_RGBINIT: u32 = 0x0000_0001;
    const CC_FULLOPEN: u32 = 0x0000_0002;
    const CC_ANYCOLOR: u32 = 0x0000_0100;

    #[link(name = "comdlg32")]
    unsafe extern "system" {
        fn ChooseColorW(param0: *mut CHOOSECOLORW) -> i32;
    }

    let mut cc = CHOOSECOLORW {
        l_struct_size: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwnd_owner: owner_hwnd,
        h_instance: 0,
        rgb_result: (b << 16) | (g << 8) | r,
        lp_cust_colors: custom.as_mut_ptr(),
        // CC_RGBINIT：用上面 rgb_result 作初始选中色；CC_FULLOPEN|CC_ANYCOLOR：展开
        // 完整取色板（与旧版 rfd ColorPicker 一致的「完整选色」体验）。
        flags: CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR,
        l_cust_data: 0,
        lpfn_hook: std::ptr::null(),
        lp_template_name: std::ptr::null(),
    };
    if unsafe { ChooseColorW(&mut cc) } == 0 {
        return None;
    }
    // COLORREF 0x00bbggrr → #rrggbb。
    let v = cc.rgb_result;
    Some(format!("#{:02x}{:02x}{:02x}", v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff))
}
