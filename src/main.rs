//! 程序入口：初始化 GPUI + GPUI-Component、注册应用级快捷键、
//! 创建主窗口并把窗口根视图包上 [`gpui_component::Root`]。
//!
//! 模块结构：
//! - [`app`]：主视图 `EditorApp`（自绘标题栏 + 菜单栏 + 选项卡栏 + 编辑区 + 可选 Markdown 预览 + 状态栏）
//! - `gpui_component::text::TextView`：Markdown 解析、排版与选择
//! - [`scrollbar`]：自绘横向滚动条（选项卡栏用）
//! - [`actions`]：应用级动作类型
//!
//! 文本（代码）编辑区使用 **gpui-component 的 `Input`（CodeEditor 模式）**：
//! 底层的 `EditorState` 用 ropey 存储文本、内置语法高亮（tree-sitter）、行号、
//! 缩进参考线、搜索面板与撤销/重做，替换掉原先自研的 `TextArea`（其换行排版
//! 在大量粘贴时曾出现“整行消失”的问题）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod config;
mod editor;
mod epub;
mod markdown;
mod pdf;
mod prof;
mod scrollbar;
mod syntax;
mod toc;

use gpui::{px, size, App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions};
use gpui_component::{Root, Theme, ThemeMode};
use gpui_component_assets::Assets;

use crate::actions::*;

/// Windows 冷启动彩色 emoji 空白根治（纯应用层，不依赖仓库外补丁）。
///
/// 机制：DWrite 的系统字体系列是**异步枚举**的。冷启动早期集合未枚举完 → 回退表不完整，
/// ❗⚠✅ 等被解析到无彩色字形的字体 → `TranslateColorGlyphRun` 返回
/// `DWRITE_E_NOCOLOR(0x8898500C)` → 首个彩色光栅空白；约 20s 后枚举完成才可能画对。
/// 这里在创建任何窗口/文本系统之前，把枚举**阻塞等完**，等效于把“等 ~20s”提前到启动
/// 时一次性付清；进程级字体集合共享，之后 gpui_windows 首访必是完整集合，三视图
/// 首帧即命中 COLR emoji 字体，无需手动滚动/点击/空等。
///
/// 可靠判据（这版做的改进，此前「连续 3 轮族数不变」会在枚举**间歇**（暂停 >300ms）误判
/// 提前放行）：枚举是成簇推进的，途中可能停很久，所以完成条件同时要求：
/// - 族数量**连续 20 轮（≈2s）稳定**（宽松窗口，跨过暂停期）；
/// - 且回退必需的 emoji 字体族（"Segoe UI Emoji"）**已进入集合**（语义上的真需求：
///   集合里没有它说明回退表不完整；它在本次实测于 index≈50 出现）。
/// 极端机器上若族名解析不到，退化为「数量稳定 ≥60 轮（≈6s）」兜底放行；总轮数上限
/// 800×100ms = 80s，不会无限卡死。`WY_ENUM_DEBUG=1` 打印每轮计数/emoji 族状态，
/// 用于确认门禁到底在哪一步放行。
#[cfg(target_os = "windows")]
fn force_dwrite_font_enumeration() {
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory5, IDWriteFontCollection1, DWRITE_FACTORY_TYPE_SHARED,
    };

    /// "Segoe UI Emoji" 是否已出现在系统字体集合里（回退表完整的必要条件）。
    fn collection_has_emoji_family(coll: &IDWriteFontCollection1) -> bool {
        let target: Vec<u16> = "Segoe UI Emoji".encode_utf16().collect();
        let n = unsafe { coll.GetFontFamilyCount() };
        for i in 0..n {
            let Ok(fam) = (unsafe { coll.GetFontFamily(i) }) else {
                continue;
            };
            let Ok(names) = (unsafe { fam.GetFamilyNames() }) else {
                continue;
            };
            let count = unsafe { names.GetCount() };
            for j in 0..count {
                let Ok(len) = (unsafe { names.GetStringLength(j) }) else {
                    continue;
                };
                // DWrite GetString 的 size 参数**含**结尾 NUL，长度必须 len+1，
                // 否则返回 E_INVALIDARG（windows-rs 0.61 直接把 slice.len 传进去）。
                let mut buf = vec![0u16; len as usize + 1];
                if unsafe { names.GetString(j, &mut buf) }.is_ok()
                    && &buf[..len as usize] == &target[..]
                {
                    return true;
                }
            }
        }
        false
    }

    let Ok(factory) = (unsafe { DWriteCreateFactory::<IDWriteFactory5>(DWRITE_FACTORY_TYPE_SHARED) })
    else {
        return;
    };
    let debug = std::env::var_os("WY_ENUM_DEBUG").is_some();
    let started = std::time::Instant::now();
    let (mut last, mut stable) = (0u32, 0u32);
    let mut emoji_loaded = false;
    let mut scanned_at: u32 = 0;
    for round in 0..800u32 {
        let mut coll: Option<IDWriteFontCollection1> = None;
        if unsafe { factory.GetSystemFontCollection(false, &mut coll, true) }.is_err() {
            return;
        }
        let Some(c) = coll else { return; };
        let n = unsafe { c.GetFontFamilyCount() };
        if n != last {
            last = n;
            stable = 0;
            emoji_loaded = collection_has_emoji_family(&c);
            scanned_at = round;
        } else {
            stable += 1;
            // 数量稳定的空窗里，每 25 轮重扫一次 emoji 族（防“族数不变但 emoji 族后到”）。
            if round >= scanned_at + 25 {
                emoji_loaded = collection_has_emoji_family(&c);
                scanned_at = round;
            }
        }
        if debug {
            eprintln!(
                "[WY-ENUM] round={round} families={n} stable={stable} emoji={emoji_loaded} t={:.1}s",
                started.elapsed().as_secs_f32()
            );
        }
        // 完成判据：数量稳定 ≥2s 且 emoji 回退族已在集合；族名解析不到的机器，
        // 稳定 ≥6s 兜底放行（数量不动 6s 基本可认定枚举已停）。
        if n > 0 && stable >= 20 && (emoji_loaded || stable >= 60) {
            if debug {
                eprintln!(
                    "[WY-ENUM] DONE families={n} emoji={emoji_loaded} t={:.1}s",
                    started.elapsed().as_secs_f32()
                );
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn main() {
    #[cfg(target_os = "windows")]
    force_dwrite_font_enumeration();

    gpui_platform::application()
        .with_assets(Assets)
        .run(|cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(980.), px(720.)), cx);

            // gpui-component 初始化会重建按键映射，因此先注册应用动作。
            cx.bind_keys([
                KeyBinding::new("ctrl-n", NewFile, None),
                KeyBinding::new("ctrl-o", OpenFile, None),
                KeyBinding::new("ctrl-s", Save, None),
                KeyBinding::new("shift-ctrl-s", SaveAs, None),
                KeyBinding::new("ctrl-w", CloseTab, None),
                KeyBinding::new("ctrl-p", TogglePreview, None),
                KeyBinding::new("ctrl-alt-w", ToggleWordWrap, None),
                KeyBinding::new("ctrl-=", IncreaseFontSize, None),
                KeyBinding::new("ctrl--", DecreaseFontSize, None),
                KeyBinding::new("ctrl-0", ResetFontSize, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);

            // 初始化 gpui-component（主题、键盘映射、Input 等）。
            // 这里强制浅色主题，与现有界面的亮色调保持一致。
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            // 「预览编辑测试」（本项目 Doc 管线 WYSIWYG）快捷键（作用域 wy-editor）。
            crate::markdown::wysiwyg::bind_keys(cx);
            // 编辑器代码高亮配色：GitHub Light（启动即安装，切换主题时 toggle_theme 会重装）
            crate::syntax::install_github_theme(cx);
            // 覆盖组件库默认粘贴：Windows 剪贴板的 CRLF 必须先转为 LF，
            // 否则回车符会作为普通字符参与代码编辑器的布局。
            cx.bind_keys([KeyBinding::new("ctrl-v", PasteNormalized, Some("Input"))]);

            let mut created: Option<gpui::Entity<app::EditorApp>> = None;
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        // 隐藏原生标题栏（它在深色系统上不受控为黑色），
                        // 改由应用内自绘标题栏（随黑白主题）。详见 app::render_title_bar。
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("GPUI 编辑器".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    |window, cx| {
                        window.set_window_title("GPUI 编辑器");
                        let view = cx.new(|cx| app::EditorApp::new(window, cx));
                        created = Some(view.clone());
                        // 窗口第一层必须是 gpui_component::Root
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .unwrap();

            // 打开后把焦点放到编辑区
            window
                .update(cx, |_root, window, cx| {
                    if let Some(view) = &created {
                        let app = view.read(cx);
                        let focus = app.editor_focus_handle(cx);
                        window.focus(&focus, cx);
                    }
                    cx.activate(true);
                })
                .unwrap();

            // Ctrl+Q 退出
            cx.on_action(|_: &Quit, cx| cx.quit());
        });
}
