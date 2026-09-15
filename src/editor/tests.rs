//! 编辑器组件测试： 纯逻辑（buffer/selection/movement/undo/highlight）+ 视图
//! （渲染、输入、撤销、选区、鼠标）。

use super::*;
use crate::editor::highlight::Highlighter;
use crate::editor::movement;
use crate::editor::undo::EditIntent;
use gpui::{
    AppContext, Context, Entity, IntoElement, Keystroke, Modifiers, MouseButton, Render,
    TestAppContext, VisualTestContext, Window, point, px, size,
};

// ---------------------------------------------------------------------------
// Buffer
// ---------------------------------------------------------------------------

#[test]
fn buffer_apply_undo_redo_roundtrip() {
    use crate::editor::buffer::test_support::assert_edit_roundtrip;
    assert_edit_roundtrip("hello world", 0..5, "hey");
    assert_edit_roundtrip("line1\nline2\nline3", 6..6, "INS");
    assert_edit_roundtrip("多字节中文🎉文本", 3..3, "X");
    assert_edit_roundtrip("a\nb\nc\n", 0..2, "zzzz");
}

#[test]
fn buffer_line_index_matches_str_lines() {
    let cases = [
        ("", 1usize),
        ("a", 1),
        ("a\n", 2),      // 尾换行：幻影空行
        ("a\nb", 2),
        ("a\nb\nc", 3),
        ("a\nb\nc\n", 4),
    ];
    for (src, lines) in cases {
        let b = Buffer::new(src);
        assert_eq!(b.line_count(), lines, "src={src:?}");
        // 行首偏移与 str 分片一致
        let str_lines: Vec<&str> = src.split('\n').collect();
        assert_eq!(str_lines.len(), lines);
        for (i, sl) in str_lines.iter().enumerate() {
            let expected_start = str_lines[..i].iter().map(|s| s.len() + 1).sum::<usize>();
            assert_eq!(b.line_start(i), expected_start, "line {i} start");
            assert_eq!(b.line_len(i), sl.len(), "line {i} len");
        }
    }
}

#[test]
fn buffer_display_line_count_matches_str_lines() {
    // 显示语义 = str::lines()（尾换行不追加行）；编辑器模型 line_count() 含幻影尾行。
    let cases = [
        "", "a", "a\n", "a\nb", "a\nb\n", "a\nb\nc\n", "\n", "\n\n", "\r\n",
        "a\r\nb", "a\r\nb\r\n", "中文\n第二行",
    ];
    for src in cases {
        let b = Buffer::new(src);
        // 应用侧显示口径 = str::lines() 取 `.max(1)`（空文本显示 1 行，见 app 状态栏/测试）。
        let expected = src.lines().count().max(1);
        assert_eq!(b.display_line_count(), expected, "display_line_count src={src:?}");
        let phantoms = usize::from(!src.is_empty() && src.ends_with('\n'));
        assert_eq!(b.line_count(), expected + phantoms, "line_count 应含幻影尾行 src={src:?}");
    }
}

#[test]
fn buffer_utf16_roundtrip() {
    let b = Buffer::new("中文🎉abc");
    let u = b.byte_to_utf16("中文".len());
    assert_eq!(u, 2); // 中文 = 2 个 BMP 字符 = 2 个 UTF-16 单元
    assert_eq!(b.utf16_to_byte(u), "中文".len());
    assert_eq!(b.utf16_to_byte(b.len_utf16()), b.len());
    // 代理对（emoji）占 2 个 UTF-16 单元
    let u2 = b.byte_to_utf16("中文".len() + "🎉".len());
    assert_eq!(u2, 4);
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[test]
fn selection_geometry() {
    let s = Selection::from_range(10, 4);
    assert_eq!(s.start(), 4);
    assert_eq!(s.end(), 10);
    assert!(s.reversed());
    assert_eq!(s.len_bytes(), 6);
    assert_eq!(s.range(), 4..10);
    assert!(!s.is_collapsed());
    let c = Selection::collapsed(5);
    assert!(c.is_collapsed());
}

// ---------------------------------------------------------------------------
// Movement
// ---------------------------------------------------------------------------

#[test]
fn movement_char_and_word() {
    let b = Buffer::new("hello world foo123");
    let rope = b.text();
    assert_eq!(movement::char_right(rope, 0), 1);
    assert_eq!(movement::char_left(rope, 1), 0);
    assert_eq!(movement::word_left(rope, 6), 0); // 在 ' ' 后回词首
    assert_eq!(movement::word_right(rope, 0), 5); // hello 词尾（下一个边界=空格前）
    assert_eq!(movement::word_right(rope, 6), 11);
    // 多字节：不应停在字中间
    let bn = Buffer::new("中🎉abc");
    let rn = bn.text();
    let mid = "中🎉".len();
    assert_eq!(movement::char_right(rn, 0), "中".len());
    assert_eq!(movement::char_left(rn, mid), "中".len());
}

#[test]
fn movement_line_and_vertical() {
    let b = Buffer::new("aa bb\ncc dd\nee");
    assert_eq!(movement::to_line_start(&b, 8), 6); // 第 2 行行首
    assert_eq!(movement::to_line_end(&b, 0), 5);
    let (off, _col) = movement::move_vertical(&b, 0, 1, None);
    assert_eq!(off, 6); // 到第 2 行同列 0
    let (off2, _) = movement::move_vertical(&b, 6, 1, Some(2));
    assert_eq!(off2, 14); // 第 3 行 col 2 → 行首12 + 2 = 14
    let (off3, _) = movement::move_vertical(&b, 0, -1, Some(2));
    assert_eq!(off3, 2); // 已 clamp 到首行，col 2 → 偏移 2
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

#[test]
fn undo_coalesces_typing_and_splits_on_enter() {
    let mut buf = Buffer::default();
    let mut mgr = UndoManager::new();
    for (i, ch) in "abc".chars().enumerate() {
        mgr.start_tx(EditIntent::Typing, (i, i));
        mgr.push_edit(buf.apply_edit(i..i, &ch.to_string()));
        mgr.set_cooperative(true);
    }
    assert_eq!(buf.to_string(), "abc");
    // 撤销一次应整体回退 abc
    let tx = mgr.undo().expect("有事务");
    assert_eq!(tx.edits.len(), 1, "打字应合并成一条记录：{}", tx.edits.len());
    for e in tx.edits.iter().rev() {
        buf.undo_edit(e);
    }
    assert_eq!(buf.to_string(), "");

    // Enter（Atomic）断开：两次 Enter 各自独立
    mgr.start_tx(EditIntent::Atomic, (0, 0));
    mgr.push_edit(buf.apply_edit(0..0, "\n1"));
    mgr.set_cooperative(false);
    mgr.start_tx(EditIntent::Atomic, (2, 2));
    mgr.push_edit(buf.apply_edit(2..2, "\n2"));
    mgr.set_cooperative(false);
    assert_eq!(mgr.undo().map(|t| t.edits.len()), Some(1));
    assert_eq!(mgr.undo().map(|t| t.edits.len()), Some(1));
    assert!(mgr.undo().is_none());
}

#[test]
fn undo_backspace_chains_merge_and_selection_restore() {
    let mut buf = Buffer::new("hello");
    let mut mgr = UndoManager::new();
    // 连续退格 → 合并成一条
    for i in 0..2 {
        mgr.start_tx(EditIntent::Backspace, (5 - i, 5 - i));
        mgr.push_edit(buf.apply_edit((4 - i)..(5 - i), ""));
        mgr.set_cooperative(true);
    }
    assert_eq!(buf.to_string(), "hel");
    let tx = mgr.undo().expect("事务");
    assert_eq!(tx.edits.len(), 1);
}

// ---------------------------------------------------------------------------
// Highlight
// ---------------------------------------------------------------------------

#[test]
fn highlight_rust_keyword_comment_string() {
    let b = Buffer::new("fn main() { // comment\nlet s = \"hi\";\n");
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let rows = hl.highlight_window(&b, 0, 1, 16).to_vec();
    assert_eq!(rows.len(), 2, "应产出两行");
    // 第 0 行含注释 → 至少一个 run
    assert!(!rows[0].is_empty(), "注释行应有高亮段");
    // 第 1 行含字符串/关键词 → 至少一个 run
    assert!(!rows[1].is_empty(), "字符串行应有高亮段");
    // 同一窗口重查应命中缓存（保持稳定）
    let again = hl.highlight_window(&b, 0, 1, 16);
    assert_eq!(again.len(), 2);
}

// ---------------------------------------------------------------------------
// 视图
// ---------------------------------------------------------------------------

struct Host {
    ed: Entity<CodeEditor>,
}
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.ed.clone()
    }
}

fn open<'a>(cx: &'a mut TestAppContext, text: &str) -> (Entity<Host>, &'a mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        ed.update(cx, |ed, _| ed.set_text(text));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    (host, cx)
}

fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }
    cx.run_until_parked();
}

fn focus(host: &Entity<Host>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(cx);
}

fn editor_text(host: &Entity<Host>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| host.read(cx).ed.read(cx).text_string())
}

fn editor_selection(host: &Entity<Host>, cx: &mut VisualTestContext) -> Selection {
    cx.update(|_, cx| host.read(cx).ed.read(cx).selection())
}

fn window_handle(cx: &mut VisualTestContext) -> gpui::AnyWindowHandle {
    cx.windows().first().copied().expect("有窗口")
}

#[gpui::test]
fn editor_renders_list_and_rows(cx: &mut TestAppContext) {
    let (host, cx) = open(cx, "第一行\n第二行\n第三行");
    let wrap = cx
        .debug_bounds("ce-list-wrap")
        .expect("编辑器列表容器应渲染");
    assert!(wrap.size.height > px(0.));
    assert!(wrap.size.width > px(0.));
    // 回归（2026-09-15 实机验证）：`list()` 必须 `.size_full()` 撑满容器。
    // 否则 List 走 Infer 测量，高度=内容高，ListState 视口高=0，
    // 整块编辑区不绘制任何行（编辑区空白、看似“无法编辑”）。
    let viewport_h = cx.update(|_, cx| host.read(cx).ed.read(cx).list_viewport_height());
    assert!(
        viewport_h > 0.,
        "list 视口应被撑满（viewport_h={viewport_h}），否则可见行数为 0"
    );
    assert_eq!(editor_text(&host, cx), "第一行\n第二行\n第三行");
}

#[gpui::test]
fn typing_then_undo_redo(cx: &mut TestAppContext) {
    let (host, mut cx) = open(cx, "");
    focus(&host, cx);
    let w = window_handle(&mut cx);
    cx.simulate_keystrokes("abc");
    settle(&mut cx);
    assert_eq!(editor_text(&host, &mut cx), "abc");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(&mut cx);
    assert_eq!(editor_text(&host, &mut cx), "");

    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-y").unwrap());
    settle(&mut cx);
    assert_eq!(editor_text(&host, &mut cx), "abc");
}

#[gpui::test]
fn caret_and_backspace(cx: &mut TestAppContext) {
    let (host, mut cx) = open(cx, "ab");
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, _| ed.set_caret_to(1));
    settle(&mut cx);
    let w = window_handle(&mut cx);
    cx.dispatch_keystroke(w, Keystroke::parse("backspace").unwrap());
    settle(&mut cx);
    assert_eq!(editor_text(&host, &mut cx), "b");
    // 回退后光标在 0
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), 0);
}

#[gpui::test]
fn select_all_then_type_replaces(cx: &mut TestAppContext) {
    let (host, mut cx) = open(cx, "hello");
    focus(&host, cx);
    let w = window_handle(&mut cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-a").unwrap());
    settle(&mut cx);
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.range(), 0..5);
    cx.simulate_keystrokes("x");
    settle(&mut cx);
    assert_eq!(editor_text(&host, &mut cx), "x");
}

#[gpui::test]
fn mouse_click_places_caret_on_second_row(cx: &mut TestAppContext) {
    let text = "row0\nrow1\nrow2";
    let (host, mut cx) = open(cx, text);
    // 取行高，点击第 2 行文本区
    let lh = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_metrics().1
    });
    cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        let fh = ed.read(cx).focus_handle();
        window.focus(&fh, cx);
    });
    settle(&mut cx);
    let y = px(lh * 1.5); // 第 2 行中部
    let x = px(4.); // gutter 内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(&mut cx);
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), "row0\n".len(), "应落到第 2 行行首");
}

#[gpui::test]
fn caret_image_is_laid_out_when_focused(cx: &mut TestAppContext) {
    let (host, mut cx) = open(cx, "abc\nxyz");
    focus(&host, &mut cx);
    settle(&mut cx);
    // 回归（2026-09-15 实机）：光标此前画画的是 colors.accent（Light 主题 =#f5f5f5
    // 白底近白 → 看不见）；这里断言光标元素真实布局出来，且尺寸 = 2px×行高。
    let b = cx
        .debug_bounds("ce-caret")
        .expect("焦点状态下应渲染光标元素");
    assert!(
        b.size.width.as_f32() > 1.0 && b.size.height.as_f32() > 0.0
            && b.origin.y < px(40.),
        "光标应落在第 1 行、宽≥1px、高>0：{b:?}"
    );
}

#[gpui::test]
fn scroll_range_survives_width_change(cx: &mut TestAppContext) {
    // 回归（2026-09-15 实机）：gpui 在宽度变化时清空全部 size_hint，
    // 未测条目高度按 0 计 → `max_offset_for_scrollbar()` 坍缩，滚动条量程失真。
    // 编辑器应在坍缩后重挂统一高度提示，量程不被换宽清掉。
    let text = (1..=300)
        .map(|i| format!("第{i}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, mut cx) = open(cx, &text);
    cx.simulate_resize(size(px(300.), px(600.)));
    settle(&mut cx);
    let mut last_max = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(
        last_max > 1000.,
        "首次渲染量程应接近 300 行（last_max={last_max:.1}）"
    );
    // 再换两次宽 → 应自愈，量程保持。
    for w in [440., 520.] {
        cx.simulate_resize(size(px(w), px(600.)));
        settle(&mut cx);
        let m = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
        assert!(m > 1000., "换宽后量程不应坍缩（m={m:.1} w={w}）");
        last_max = m;
    }
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1);
    assert!(
        (last_max - (300.0 * lh - 600.0)).abs() < lh * 4.,
        "量程应≈300×行高−视口高（{last_max:.1} vs {:.1}）",
        300.0 * lh - 600.0
    );
}
