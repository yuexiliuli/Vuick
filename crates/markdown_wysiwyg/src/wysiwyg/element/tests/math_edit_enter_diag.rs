// Math 编辑卡内嵌 `editor::CodeEditor` 的 Enter / Shift+Enter 行为矩阵与
// NoAction 放行机制的**正式回归**（由诊断探针规范化而来，2026-11-13）。
//
// 背景机制（诊断 agent 已 headless 实证，本文件为机制验证 + 行为快照）：
// - gpui 按键**先动作层后 key_down**（window.rs `dispatch_key_event`：`for binding in
//   match_result.bindings { dispatch_action_on_node }` → 之后才 `finish_dispatch_key_event`
//   派发 key_down）；动作层 Bubble 首个匹配 listener 默认 stop（window.rs:6745 先置
//   `cx.propagate_event=false` 再调 listener）→ `return`=吞掉 key_down。
// - Math 卡内嵌 CodeEditor 的 Enter 换行在 **key_down** 层（editor/src/view/editing.rs:56-63，
//   `if k.key=="enter" { insert_newline; stop_propagation }`）；editor 不挂 `on_action(&Enter)`
//  （render.rs:608-612）。
// - 修复前：Math 卡聚焦时 dispatch 路径含 WY 根 `key_context("wy-editor")`
//   +`on_action(&wysiwyg::Enter→newline)`（wysiwyg/mod.rs:561,615 + bind_keys mod.rs:519），
//   Enter 动作层先命中并 stop → CodeEditor 的 key_down 换行永不执行、且换行错插到
//   **文档** WY caret 处；Shift+Enter 无 action 绑定 → 走 key_down → 正常换行。
// - 修复机制（本次落地）：Math 卡内层 `key_context(MATH_EDIT_CONTEXT)` + 绑
//   `enter → NoAction`（wysiwyg/mod.rs `MATH_EDIT_CONTEXT` + `bind_keys` + math_edit.rs 承载体
//   div）。`NoAction` 在 `resolve_bindings_for_input`（keymap.rs:229-254）里**按深度优先压制**
//   等/弱优先级的 enter 绑定（meta 0），使 `match_result.bindings` 为空 → 动作层放空 →
//   key_down 照常派发 → CodeEditor 原样处理 Enter（含 IME 组合路径经由 input handler，
//   与主编辑器完全同路径）。卡片内聚焦须经本 context，否则 Enter 仍漏到 WY 根。
//
// ⚠️ 本文件经 include! 拼接进 `element::tests` 共享模块：宿主 / 常用 gpui 类型（Context/
// Entity/TestAppContext/…）由其它测试文件（byte_map.rs 等）已导入，**不得重复 import**；
// trait 导入（EntityInputHandler / ImeTextHost）须写在函数体内局部 use，避免模块级重影。

struct WyDiagHost {
    ed: gpui::Entity<WyEditor>,
}
impl gpui::Render for WyDiagHost {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        self.ed.clone()
    }
}

#[gpui::test]
fn diag_math_edit_enter_newlines_formula_no_doc_leak(cx: &mut TestAppContext) {
    // 回归（修复后目标行为）：Math 卡内嵌 CodeEditor 聚焦、公式 caret 在末尾（无组合），
    // 按 Enter → CodeEditor key_down 在**公式**末尾插换行（"F(a)"→"F(a)\n"）、
    // **文档正文不变**（无 WY caret 处换行泄漏）。
    // 机制快照对照：修复前同场景 Enter 不改公式（动作层吞）+ 文档泄漏 \n（见历史
    // `diag_math_edit_enter_leaks_newline_into_doc` 行为①，已被本测试取代）。
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "para one\n$$\nF(a)\n$$\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyDiagHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| ed.update(cx, |e, _| e.set_caret(4)));
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.open_math_edit(1, window, cx)));
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        window.focus(&input.read(cx).focus_handle(), cx);
    });
    // 内嵌编辑器光标置公式末尾（open 默认 caret=0 → 换行落公式头部，断言不直观）。
    vcx.update(|_w, cx| {
        ed.read(cx)
            .math_edit_input()
            .unwrap()
            .update(cx, |ed, _| ed.set_caret_to(4));
    });
    let wh = vcx.windows().first().copied().unwrap();
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let after = vcx.update(|_w, cx| ed.read(cx).content());
    let form_after = vcx
        .update(|_w, cx| ed.read(cx).math_edit_input().map(|i| i.read(cx).text_string()));
    assert_eq!(
        form_after.as_deref(),
        Some("F(a)\n"),
        "Enter 应在公式末尾插换行（CodeEditor key_down 生效）：form={form_after:?}"
    );
    assert!(
        !after.starts_with("para\n one"),
        "Enter 不得向文档 WY caret=4 处泄漏换行：{after:?}"
    );
    // 文档应仅经 `apply_math_block_edit` 写回反映公式多一行（正文行整体替换），无泄漏。
    assert_eq!(
        after.as_str(),
        "para one\n$$\nF(a)\n\n$$\n尾\n",
        "文档应仅把块正文替换为多行（无 WY caret 处泄漏）: {after:?}"
    );
}

#[gpui::test]
fn diag_math_edit_shift_enter_newlines_formula_only(cx: &mut TestAppContext) {
    // 机制对照（回归）：Math 卡聚焦时按 Shift+Enter → 公式换行、文档**不**泄漏
    // （只命中 base::actions::Enter，WY 根不监听 → key_down 照常派发）。
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "para one\n$$\nF(a)\n$$\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyDiagHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| ed.update(cx, |e, _| e.set_caret(4)));
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.open_math_edit(1, window, cx)));
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        window.focus(&input.read(cx).focus_handle(), cx);
    });
    // 把内嵌编辑器光标放到公式末尾（open 默认 caret=0，否则换行落在公式头部不易断言）。
    vcx.update(|_w, cx| {
        ed.read(cx)
            .math_edit_input()
            .unwrap()
            .update(cx, |ed, _| ed.set_caret_to(4));
    });
    let wh = vcx.windows().first().copied().unwrap();
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("shift-enter").unwrap());
    vcx.run_until_parked();
    let after = vcx.update(|_w, cx| ed.read(cx).content());
    let form_after = vcx
        .update(|_w, cx| ed.read(cx).math_edit_input().map(|i| i.read(cx).text_string()));
    // 文档**合法**变化：公式多行后 `apply_math_block_edit` 把块正文行整体替换为 "F(a)\n"。
    assert_eq!(form_after.as_deref(), Some("F(a)\n"), "Shift+Enter 应在公式内换行：form={form_after:?}");
    assert!(
        !after.starts_with("para\n one"),
        "Shift+Enter 不得向文档 WY caret=4 处泄漏换行：{after:?}"
    );
    assert_eq!(
        after.as_str(),
        "para one\n$$\nF(a)\n\n$$\n尾\n",
        "文档应仅把块正文替换为多行（无 WY caret 处泄漏）: {after:?}"
    );
}

#[gpui::test]
fn diag_math_edit_enter_during_ime_composition_no_doc_leak(cx: &mut TestAppContext) {
    // 回归（修复后目标行为）：Math 卡聚焦并存在 IME 组合（marked 文本）时按 Enter →
    // NoAction 放行 key_down → 与主编辑器契约一致（换行插在组合尾部后、悬置 marked 不
    // 被本修改反噬）、且**不向文档泄漏换行**。真实 Windows IME 在组合态通常把 Enter
    // 吞掉作「确认候选」另发 commit（input handler），key_down 不进码；本断言只记录
    // 「若 OS 转发 Enter」时的行为边界与主编辑器一致。
    // 机制快照对照：修复前同场景 Enter 被动作层吞、组合悬置未确认且文档泄漏 \n
    // （历史 `diag_math_edit_enter_during_ime_composition_current` 行为③，已被本测试取代）。
    use gpui::EntityInputHandler as _;
    use ime::ImeTextHost as _;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "para one\n$$\nF(a)\n$$\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyDiagHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| ed.update(cx, |e, _| e.set_caret(4)));
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.open_math_edit(1, window, cx)));
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        window.focus(&input.read(cx).focus_handle(), cx);
    });
    // 模拟 IME 组合：编辑器 open 默认 caret=0 → 在公式**头部** mark "ni"（marked=0..2，
    // 文本 = "niF(a)"）。
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("输入区");
        input.update(cx, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "ni", None, window, cx)
        });
    });
    let wh = vcx.windows().first().copied().unwrap();
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let (form_after, marked_after, doc_after) = vcx.update(|_w, cx| {
        let input = ed.read(cx).math_edit_input().unwrap();
        (
            input.read(cx).text_string(),
            input.read(cx).marked_bytes(),
            ed.read(cx).content(),
        )
    });
    assert_eq!(form_after.as_str(), "ni\nF(a)", "组合中按 Enter：换行插在组合尾部后（主编辑器契约一致）");
    assert_eq!(marked_after, Some(0..2), "悬置的 marked_range 未被清（与主编辑器同款语义）");
    // 文档仅经公式写回反映形状变化（块正文 "ni\nF(a)"），**无 WY caret=4 处泄漏**。
    assert_eq!(
        doc_after.as_str(),
        "para one\n$$\nni\nF(a)\n$$\n尾\n",
        "文档不得被 WY 层泄漏换行（关键验收）：{doc_after:?}"
    );
    assert!(!doc_after.starts_with("para\n one"), "WY caret 处不得出现换行");
}

#[gpui::test]
fn probe_noaction_enter_during_ime_composition(cx: &mut TestAppContext) {
    // IME 风险探针：存在组合（marked）时按 Enter 且 NoAction 已放行 key_down →
    // CodeEditor.on_key_down 的 enter 直插换行、marked_range 悬置。这正是**主编辑器当前
    // 同款语义**（editing.rs:52-63 注释：空 Enter key 在 IME/字体路径之前优先处理）——真实
    // Windows IME 在组合态把 Enter 吞掉作「确认候选」另发 commit（input handler），
    // key_down 不进码，故不会走到这里；本探针只记录「若 OS 转发 Enter」时的行为边界。
    use gpui::EntityInputHandler as _;
    use gpui::NoAction;
    use ime::ImeTextHost as _;
    cx.update(crate::test_support::bind_local_keymaps);
    cx.update(|cx| {
        cx.bind_keys([gpui::KeyBinding::new("enter", NoAction, Some("Input"))]);
    });
    let text = "para one\n$$\nF(a)\n$$\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyDiagHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| ed.update(cx, |e, _| e.set_caret(4)));
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.open_math_edit(1, window, cx)));
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        window.focus(&input.read(cx).focus_handle(), cx);
    });
    // 组合 "ni" 于公式头（caret=0 → marked 0..2，文本 "niF(a)"）。
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().unwrap();
        input.update(cx, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "ni", None, window, cx)
        });
    });
    let wh = vcx.windows().first().copied().unwrap();
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let (form_after, marked_after, doc_after) = vcx.update(|_w, cx| {
        let input = ed.read(cx).math_edit_input().unwrap();
        (
            input.read(cx).text_string(),
            input.read(cx).marked_bytes(),
            ed.read(cx).content(),
        )
    });
    assert_eq!(form_after.as_str(), "ni\nF(a)", "若 OS 转发 Enter：换行插在组合尾部后");
    assert_eq!(marked_after, Some(0..2), "悬置的 marked_range 未被清（编辑器层既有语义）");
    // 文档仅经公式写回反映形状变化（块正文 "ni\nF(a)"），**无 WY caret=4 处泄漏**。
    assert_eq!(
        doc_after.as_str(),
        "para one\n$$\nni\nF(a)\n$$\n尾\n",
        "文档不得被 WY 层泄漏换行（关键验收）：{doc_after:?}"
    );
    assert!(!doc_after.starts_with("para\n one"), "WY caret 处不得出现换行");
}

#[gpui::test]
fn probe_noaction_input_context_frees_enter_to_keydown(cx: &mut TestAppContext) {
    // 机制探针（生产零改动等价形态）：在 "Input" 上下文追加 `enter → NoAction` → 压制
    // 外层 wy-editor enter 绑定 → `match_result.bindings` 为空 → key_down 照常派发 →
    // CodeEditor 公式换行。生产落地 = 把该 NoAction 绑到 **Math 卡专属内层 key_context**
    // （`MATH_EDIT_CONTEXT`，作用域收敛到卡内、不污染主编辑器）；此处用 "Input" 只是
    // 免改生产的机制验证（`diag_math_edit_enter_newlines_formula_no_doc_leak` 走生产路径）。
    use gpui::NoAction;
    cx.update(crate::test_support::bind_local_keymaps);
    cx.update(|cx| {
        cx.bind_keys([gpui::KeyBinding::new("enter", NoAction, Some("Input"))]);
    });
    let text = "para one\n$$\nF(a)\n$$\n尾\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyDiagHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    let ed = host.read_with(vcx, |h, _| h.ed.clone());
    vcx.update(|_, cx| ed.update(cx, |e, _| e.set_caret(4)));
    vcx.update(|window, cx| ed.update(cx, |e, cx| e.open_math_edit(1, window, cx)));
    vcx.update(|window, cx| {
        let input = ed.read(cx).math_edit_input().expect("应有输入区");
        window.focus(&input.read(cx).focus_handle(), cx);
    });
    // 内嵌编辑器光标置公式末尾（open 默认 caret=0 → 换行落头部，断言不直观）。
    vcx.update(|_w, cx| {
        ed.read(cx)
            .math_edit_input()
            .unwrap()
            .update(cx, |ed, _| ed.set_caret_to(4));
    });
    let wh = vcx.windows().first().copied().unwrap();
    vcx.dispatch_keystroke(wh, gpui::Keystroke::parse("enter").unwrap());
    vcx.run_until_parked();
    let after = vcx.update(|_w, cx| ed.read(cx).content());
    let form_after = vcx
        .update(|_w, cx| ed.read(cx).math_edit_input().map(|i| i.read(cx).text_string()));
    assert_eq!(
        form_after.as_deref(),
        Some("F(a)\n"),
        "探针：Enter 应被 CodeEditor 处理为公式换行：form={form_after:?}"
    );
    assert!(
        !after.starts_with("para\n one"),
        "探针：Enter 不得向文档 caret=4 处泄漏换行：{after:?}"
    );
    // 文档应仅经 `apply_math_block_edit` 写回反映出公式多了一行（正文行整体替换）。
    assert_eq!(
        after.as_str(),
        "para one\n$$\nF(a)\n\n$$\n尾\n",
        "探针：文档仅反映公式写回（多一行正文），无 caret 处泄漏: {after:?}"
    );
}
