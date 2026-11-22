// 「帮助 → 关于」面板测试（headless）：`about_text` 纯函数 + 面板打开/关闭/复制。
//
// 面板生命周期走 `EditorApp::open_about_panel` / `close_about_panel`（帮助菜单「关于」项
// 就调用前者）。「复制 / 关闭」走真实按钮点击（headless 元素树驱动）。菜单点击路径的
// 完整下拉 UI 驱动在 shell 无既有先例，此处以宿主方法覆盖打开的接线侧（菜单 handler 与
// 测试调用同一方法，接线正确性由编译面保证）；真机观感留待 GUI 冒烟（见报告遗留栏）。

use crate::app::about::about_text;
use gpui::Modifiers;

/// `about_text` 应含当前信息键且值非空（纯函数回归护栏；同时证明 build.rs 的 `VUICK_*`
/// env 注入成功、`tray::os_version` 参与拼接、构建档/目标/位宽正确拼接）。用户已删除
/// GPUI/BuildId 显示行，此处只断言保留的核心键集。
#[test]
fn about_text_has_core_fields_nonempty() {
    let t = about_text();
    for key in ["Rust:", "构建:", "OS:"] {
        assert!(t.contains(key), "about_text 缺字段 {key}：\n{t}");
    }
    // 首行 = 应用名；版本号在「Version:」行。
    assert!(t.starts_with("Vuick\n"), "首行应为 Vuick：\n{t}");
    assert!(
        t.contains(&format!("Version: {}", env!("CARGO_PKG_VERSION"))),
        "Version 行应含版本号：\n{t}"
    );
    // 构建行 = 档位 · 目标（本机 debug 构建 → `debug`）。
    assert!(
        t.contains("构建: debug · ") || t.contains("构建: release · "),
        "构建行应含 debug/release 与目标：\n{t}"
    );
    // OS 行值非空（Windows = `Windows NT …`；其它平台 = `Other OS`）。
    let os = t.split("OS: ").nth(1).unwrap_or("").trim();
    assert!(!os.is_empty(), "OS 行值不应为空：\n{t}");
}

/// 打开宿主 → `open_about_panel` → 面板置 `Some`；`close_about_panel` → 回 `None`
/// （`DismissEvent` 经订阅清引用释放实体）。
#[gpui::test]
async fn about_panel_open_and_close(cx: &mut TestAppContext) {
    let (app, cx) = add_app(cx);
    let opened = cx.update(|_w, cx| app.read(cx).about_panel.is_some());
    assert!(!opened, "初始不应有关于面板");
    // 「关于」菜单项调用的正是此方法（menu.rs help_items → open_about_panel）。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_about_panel(window, cx));
    });
    let opened = cx.update(|_w, cx| app.read(cx).about_panel.is_some());
    assert!(opened, "open_about_panel 后应有面板实体");
    // 重复打开幂等（`open_about_panel` 首行守卫）：仍只有一个单槽，不叠多层。
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_about_panel(window, cx));
    });
    let opened = cx.update(|_w, cx| app.read(cx).about_panel.is_some());
    assert!(opened, "重复打开应保持面板打开（单槽不叠）");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.close_about_panel(window, cx));
    });
    cx.run_until_parked();
    let opened = cx.update(|_w, cx| app.read(cx).about_panel.is_some());
    assert!(!opened, "close_about_panel 后关于面板应清空");
}

/// 复制按钮：渲染面板 → 点「复制」→ 剪贴板 == `about_text()`（headless 测试平台
/// 写入即读回）→ 按钮「已复制」反馈置位；点「关闭」→ 面板清空。
#[gpui::test]
async fn about_copy_writes_clipboard_and_close_dismisses(cx: &mut TestAppContext) {
    let (app, cx) = add_app(cx);
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_about_panel(window, cx));
    });
    // 渲染一帧让面板（deferred 全窗 occlude 居中卡）进入元素树，按钮可命中
    // （同 recent_panel 的 pump_frame 姿势：draw + run_until_parked）。
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
    let copy_btn = cx
        .debug_bounds("about-copy")
        .expect("复制按钮应已渲染（headless 元素树）");
    cx.simulate_click(copy_btn.center(), Modifiers::default());
    cx.run_until_parked();
    // 效果断言（K37）：剪贴板真的写入了 == 面板复制文本，而非只渲染通过。
    let clip = cx.read_from_clipboard().and_then(|i| i.text());
    assert_eq!(clip, Some(about_text()), "复制内容应 == about_text()");
    // 复制按钮反馈态翻到「已复制」。
    let copied = cx.update(|_w, cx| {
        app.read(cx)
            .about_panel
            .as_ref()
            .is_some_and(|p| p.read(cx).copied())
    });
    assert!(copied, "点复制后按钮应置「已复制」反馈态");

    // 点「关闭」→ DismissEvent → 宿主清引用（面板关闭）。
    let close_btn = cx
        .debug_bounds("about-close")
        .expect("关闭按钮应已渲染");
    cx.simulate_click(close_btn.center(), Modifiers::default());
    cx.run_until_parked();
    let opened = cx.update(|_w, cx| app.read(cx).about_panel.is_some());
    assert!(!opened, "点关闭后关于面板应关闭");
}
