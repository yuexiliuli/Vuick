// Tab / Shift+Tab 键在源码编辑区（CodeEditor）的行缩进语义（E1/E2）。
//
// E1：无选区、光标在字符前按 Tab → 在光标处**插入一层可见缩进**（4 空格 = `tab_size`，
//     与 edit.rs 的 H2 修复同源：真机 DirectWrite 把 `\t` 排成零宽，故不插 `\t`）。
// E2：选区覆盖一行/多行按 Tab → **每行行首各插 4 空格缩进**（缩进选中几行），
//     且**绝不删除/吞掉选中行**（修复前：`tab(false)` 把整个选区替换成一个 `\t`，
//     「选中一行再 Tab 行消失」= 数据破坏）；无选区光标在行内 → 只在光标处插缩进。
// Shift+Tab 保持既有 OutdentInline「反缩进」语义不回归（吃掉 ≤tab_size 前导空格/Tab）。
//
// 复用 `tests/view.rs`（`open`/`focus`/`editor_text`/`editor_selection`/
// `window_handle`/`settle`）与 `tests/logic.rs` 的 import（`Keystroke`/`TestAppContext`/
// `VisualTestContext`/`Entity` 等已在同一测试模块作用域）。

/// t1：无选区光标在字符前按 Tab → 光标处插入 4 空格缩进，光标落在其后。
#[gpui::test]
fn tab_caret_inserts_tab_char(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "abcdef");
    focus(&host, cx);
    let w = window_handle(cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(2));
    cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "ab    cdef",
        "无选区光标前应插入一层（4 空格）可见缩进"
    );
    assert_eq!(
        editor_selection(&host, cx).head(),
        6,
        "光标应落在插入的 4 空格之后（2 + 4）"
    );
}

/// t2-全行：整选一行再 Tab → 该行行首插 4 空格缩进，**行不得消失**
/// （修复前：整行被替换成一个 Tab）。
#[gpui::test]
fn tab_single_selected_line_indents_and_keeps_line(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\n");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 0..4 = "aaa\n"（整选第 0 行，含换行）。
    ed.update(cx, |ed, ecx| ed.set_selection_range(0..4, ecx));
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "    aaa\nbbb\n",
        "选中整行按 Tab 应在行首插 4 空格缩进、原行文字完整保留（不得吞行）"
    );
}

/// t2-多行：选区覆盖 2 行 → 每行行首各插 4 空格缩进，选中文本逐字节保留；
/// 缩进后选区仍覆盖同样两行。
#[gpui::test]
fn tab_selected_lines_indent_each_line_and_keep_selection(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\nccc\n");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 0..8 = "aaa\nbbb\n"（整选第 0、1 两行）。
    ed.update(cx, |ed, ecx| ed.set_selection_range(0..8, ecx));
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "    aaa\n    bbb\nccc\n",
        "选中两行按 Tab 应每行行首各插 4 空格缩进，内容绝不被吞"
    );
    // 每行被其前面的缩进右移 4 字节：0..8 → 4..16。
    let sel = editor_selection(&host, cx);
    assert_eq!(sel.range(), 4..16, "缩进后选区应仍覆盖原两行");
}

/// t2-光标在行内（无选区）：只在光标处插 4 空格缩进，不缩进整行、其它行不动。
#[gpui::test]
fn tab_caret_inside_line_does_not_indent_whole_line(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\n");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(1));
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "a    aa\nbbb\n",
        "光标在行内按 Tab 应只在光标处插缩进，不缩进整行、其它行不动"
    );
}

/// Shift+Tab：既有「反缩进」语义（OutdentInline）不回归——光标行行首吃掉一个 Tab。
#[gpui::test]
fn shift_tab_outdents_caret_line(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "\taaa\n\t\tbbb\n");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(0));
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("shift-tab").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "aaa\n\t\tbbb\n",
        "Shift+Tab 应吃掉光标行行首的一个 Tab"
    );
}

/// 缩进为一批原子事务：单次撤销整组回退（编辑器 undo 回归护栏）。
#[gpui::test]
fn tab_indent_undo_restores_selected_lines(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "aaa\nbbb\nccc\n");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, ecx| ed.set_selection_range(0..8, ecx));
    let w = window_handle(cx);
    cx.dispatch_keystroke(w, Keystroke::parse("tab").unwrap());
    settle(cx);
    assert_eq!(editor_text(&host, cx), "    aaa\n    bbb\nccc\n");
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(cx);
    assert_eq!(
        editor_text(&host, cx),
        "aaa\nbbb\nccc\n",
        "撤销应把两个 Tab 作为整组回退"
    );
}
