//! 主窗口尺寸记忆（主窗口 resize 自动记录窗口化尺寸 + 启动恢复）集成测试。
//!
//! 断言走生产路径逐字一致（`EditorApp` / `EditorAppRegistry` 经 `common::*` 重导出
//! 访问）；`common::init_test` 注入 `VUICK_TEST` → `persist_prefs` 恒短路不写盘
//! （K8：shell 测试 config 隔离由 test-mode feature 保证），断言内存字段即可。

mod common;

use common::*;

/// 只读某窗口句柄的当前 bounds.size（测试平台 `open_window` 按 `WindowBounds` 落位）。
fn window_size(cx: &mut TestAppContext, wh: AnyWindowHandle) -> Option<gpui::Size<Pixels>> {
    cx.update(|cx| wh.update(cx, |_, w, _| w.bounds().size).ok())
}

/// 主窗口（`startup=true` 构造 = `EditorApp::new`）resize 后自动记录窗口化尺寸到
/// `model.main_window_size`（端到端：`simulate_resize` 走真实 resize 回调 → bounds
/// 观察者路径，镜像 `media_window_resize_records_on_new_media_window` 同款姿势）。
/// 判别式：撤掉主窗口 `observe_window_bounds` 记录逻辑 → model 不变（红）。
#[gpui::test]
async fn main_window_resize_records_size(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let recorded = cx.update(|_w, cx| app.read(cx).model.borrow().main_window_size);
    assert_eq!(
        recorded,
        Some(gpui::size(px(1280.), px(800.))),
        "主窗口 resize 后应自动记录窗口化尺寸"
    );
}

/// 记录方法最小尺寸护栏 + 去重（域逻辑 `shell_core::actions::record_main_window_size`，
/// 宿主 `record_main_window_size` 直驱，镜像媒体窗口尺寸记录单测同款姿势）：极端小
/// resize 被 clamp 到最小护栏；同尺寸重复不覆盖（值稳定）。
#[gpui::test]
async fn main_window_record_clamps_and_dedups(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.update(|_window, cx| {
        app.update(cx, |app, cx| {
            // 正常 resize 记录。
            app.record_main_window_size(gpui::size(px(1280.), px(800.)), cx);
            // 同尺寸重复 → 不覆盖（最终仍 1280×800）。
            app.record_main_window_size(gpui::size(px(1280.), px(800.)), cx);
            // 极端小尺寸 → clamp 到最小尺寸护栏（640×480 起）。
            app.record_main_window_size(gpui::size(px(100.), px(100.)), cx);
        });
    });
    let recorded = cx.update(|_w, cx| app.read(cx).model.borrow().main_window_size);
    assert_eq!(
        recorded,
        Some(gpui::size(px(640.), px(480.))),
        "极端小 resize 应 clamp 到最小尺寸护栏"
    );
}

/// 主窗口在**全屏**状态下 resize 不记录「窗口化尺寸」（max/fullscreen 跳过分支）。
/// 测试平台 `is_maximized()` 恒 `false`（gpui `platform/test/window.rs:344`）→ 最大化臂
/// 无法 headless 模拟；全屏经 `toggle_fullscreen` 真实翻转（K17），与最大化共用同一
/// `||` 闸门 → 覆盖「去掉 max/fullscreen 跳过分支则被记录（红）」判别式。
#[gpui::test]
async fn main_window_skips_record_when_fullscreen(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1280.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert_eq!(
        cx.update(|_w, cx| app.read(cx).model.borrow().main_window_size),
        Some(gpui::size(px(1280.), px(800.))),
        "前置：窗口化 resize 应已记录"
    );
    // 进入全屏 → resize → observe 回调因 `is_fullscreen()==true` 跳过记录。
    cx.update(|window, _| window.toggle_fullscreen());
    cx.simulate_resize(gpui::size(px(900.), px(600.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert_eq!(
        cx.update(|_w, cx| app.read(cx).model.borrow().main_window_size),
        Some(gpui::size(px(1280.), px(800.))),
        "全屏期间 resize 不应覆盖记录（保持全屏前窗口化尺寸）"
    );
}

/// 启动恢复：`shell::app::main_window_initial_size`（`src/main.rs` 与测试共用**同一真值
/// 函数**）按记录尺寸覆盖默认 980×720；主窗口以该尺寸打开（测试平台 `open_window` 按
/// `window_bounds` 落位，媒体窗口尺寸断言同款姿势）→ `window.bounds().size == 记录值`。
/// 判别式：撤掉 `main_window_initial_size` 的记录分支（恒回默认）→ 断言红。
#[gpui::test]
async fn main_window_startup_restores_recorded_size(cx: &mut TestAppContext) {
    init_test(cx);
    // 未记录 → 默认 980×720。
    assert_eq!(
        shell::app::main_window_initial_size(None),
        gpui::size(px(980.), px(720.)),
        "未记录时主窗口用默认 980×720"
    );
    // 记录在案 → 按记录尺寸（main.rs 启动恢复的同一真值）。
    let recorded: Option<(f32, f32)> = Some((1280.0, 800.0));
    let init_size = shell::app::main_window_initial_size(recorded);
    assert_eq!(
        init_size,
        gpui::size(px(1280.), px(800.)),
        "有记录时主窗口初始尺寸取记录值"
    );
    // 以该尺寸开主窗口（模拟 main.rs 启动路径）→ 平台按 window_bounds 落位。
    let wh: AnyWindowHandle = cx
        .open_window(init_size, |window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            ui::root::Root::new(app, window, cx)
        })
        .into();
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert_eq!(
        window_size(cx, wh),
        Some(init_size),
        "主窗口应按记录尺寸打开（启动恢复）"
    );
}

/// 读路径最小尺寸 clamp：config 里**手写** < 最小护栏的有效尺寸 → 启动初始尺寸被 clamp
/// 到 [`shell_core::actions::MIN_MAIN_WIN_W/H`]（写路径保存时恒 ≥MIN，此处只兜底手改/
/// 旧版配置，镜像媒体窗口 `host_impl.rs` 读记录尺寸的 clamp 姿势）。
/// 判别式：去掉 `main_window_initial_size` Some 分支的 `.max` clamp → 返回手写小值（红）。
#[gpui::test]
async fn main_window_startup_clamps_undersized_record(cx: &mut TestAppContext) {
    init_test(cx);
    // 手写 < 最小护栏的有效正数 → 初始尺寸被 clamp 到最小护栏。
    let undersized: Option<(f32, f32)> = Some((400.0, 300.0));
    let init_size = shell::app::main_window_initial_size(undersized);
    assert_eq!(
        init_size,
        gpui::size(
            px(shell_core::actions::MIN_MAIN_WIN_W),
            px(shell_core::actions::MIN_MAIN_WIN_H),
        ),
        "低于最小护栏的尺寸应 clamp 到最小尺寸"
    );
    // 以 clamp 后尺寸开主窗口 → 平台按 window_bounds 落位（与启动恢复同款姿势）。
    let wh: AnyWindowHandle = cx
        .open_window(init_size, |window, cx| {
            let app = cx.new(|cx| EditorApp::new(window, cx));
            ui::root::Root::new(app, window, cx)
        })
        .into();
    for _ in 0..3 {
        cx.run_until_parked();
    }
    assert_eq!(
        window_size(cx, wh),
        Some(init_size),
        "clamp 后主窗口按最小尺寸打开"
    );
}
