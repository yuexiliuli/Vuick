
// ---------------------------------------------------------------------------
// 查找（find）/ 跳转（goto）卡片输入框取得焦点时，编辑区光标应停闪（红测）
// ---------------------------------------------------------------------------
// 用户报告 bug：编辑器里 Ctrl+F 打开查找卡片后，**点搜索框 → 两个光标一起闪**
// （期望只搜索框光标闪，编辑区光标应停闪 / 隐藏）。
//
// 诊断结论（详见主会话根因，本测试为干净的复现红测）：
//  - `render_find_card`（render.rs）把 find/goto 输入框（`ui::InputState`）挂在
//    `core` div 之下，而 `core` 用 `.track_focus(&self.focus_handle)`（render.rs:144）
//    —— 输入框焦点节点是编辑区焦点节点的**后代**；
//  - `open_find`/`open_goto`（find_goto.rs:98/388）会 `window.focus(&input_fh)`，
//    把窗口焦点移到输入框 → 编辑区 `focus_handle.is_focused(window)` 变 false；
//  - 但 `CodeEditor` 的 `on_blur`（interaction.rs:41）**永不触发**：render.rs:573-574
//    用 `let _ = cx.on_focus / on_blur(...)` 注册，返回的 `Subscription` 当场被 drop，
//    gpui `SubscriberSet`（subscription.rs Drop→unsubscribe）立刻把它移除 —— 聚焦/失焦
//    监听是死代码，实证焦点监听器集合为空（`total=0`）；
//  - 于是 `caret.focused` 只能靠 render.rs:579 的补同步**置 true**（`is_focused` 为真时），
//    没有任何路径把它置 false（`sync_render_state` 缺 wysiwyg/core.rs:910-915 那种
//    `!is_focused && focused → on_blur` 对称回退）→ 输入框持焦期间编辑区光标一直闪。
//  ⚠️ headless 下演练真实焦点事件还需先把窗口置为活动（`window.activate_window()`）：
//  window.rs 焦点事件派发前用 `window_active` 门控把路径清零（window.rs:3554-3564），
//  测试 `open()` 默认不 activate → 空跑。本红测先 activate 以贴近真实活动窗口。
//
// 前置约定（复用 `tests/view.rs` / `tests/find.rs` 既有 helper）：
//  `open` / `focus` / `settle` / `ed_of` / `open_find` 在本模块作用域内
//  （include 顺序在 view.rs、find.rs 之后）。

/// 用户可观测语义：Ctrl+F 打开查找卡片、焦点落在 find 输入框之后，编辑区光标应
/// **停闪（隐藏）**——只输入框光标闪。当前 bug：编辑区 `caret.focused` 在输入框
/// 持焦期间仍为 true（两个光标一起闪）。
///
/// 判别式（撤掉修复=红）：输入框持焦时编辑区必 `is_focused=false`，此时
/// `caret.focused` 若仍 true 即复现本 bug；修复后（补 blur 回退或修活焦点监听）
/// 这里应 false。
#[gpui::test]
fn find_input_focus_hides_editor_caret(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aa bb aa");
    let ed = ed_of(&host, cx);

    // 模拟真实活动窗口：gpui 焦点事件路径会先按 `window_active` 把路径清零
    // （window.rs:3554-3564），测试 `open()` 默认不 activate → 真实焦点事件在
    // headless 不被派发。真实 app 用户正在操作 = 活动窗口，这里对齐它。
    cx.update(|window, _| window.activate_window());
    for _ in 0..3 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }

    // 对照分支 1（正常参照）：编辑区持焦时 `caret.focused == true`（光标该闪），
    // 且这是真实焦点转移语义的一部分（不是空断言）。
    focus(&host, cx);
    let focused_ed = cx.update(|_, cx| ed.read(cx).test_caret_focused());
    assert!(
        focused_ed,
        "前置（对照）：编辑区持焦时 caret.focused 应为 true（实际 {focused_ed}）"
    );

    // 打开查找卡片 → `open_find` 自动聚焦输入框（window.focus(find_input)）。
    let input = open_find(&host, cx);
    // 证明焦点真的落在输入框（真实焦点转移前置，非空断言）。
    let input_focused = cx.update(|window, cx| {
        let i = input.read(cx).focus_handle(cx);
        window.focused(cx) == Some(i)
    });
    assert!(
        input_focused,
        "前置（焦点转移）：打开查找卡片后输入框应取得焦点"
    );
    // 编辑区自身不再是窗口焦点叶子。
    let ed_now_focused = cx.update(|window, cx| {
        let fh = ed.read(cx).focus_handle();
        window.focused(cx) == Some(fh)
    });
    assert!(
        !ed_now_focused,
        "前置（焦点转移）：输入框持焦期间编辑区不应再是焦点叶子"
    );

    // 多刷几帧，让 gpui 焦点 diff / 每帧补同步 / 闪烁循环全面生效，再判最终态。
    for _ in 0..6 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }
    cx.run_until_parked();

    // 对照分支 2（本 bug 断言）：输入框持焦 ⇒ 编辑区光标应停闪。
    let focused_input = cx.update(|_, cx| ed.read(cx).test_caret_focused());
    assert!(
        !focused_input,
        "输入框持焦期间编辑区光标应停闪（caret.focused 应为 false，实际 {focused_input} = 两个光标一起闪的 bug）"
    );
    let _ = input;
}
