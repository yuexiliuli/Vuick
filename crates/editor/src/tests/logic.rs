
use super::*;
use crate::editor::fold::Fold;
use crate::editor::highlight::Highlighter;
use crate::editor::movement;
use crate::editor::undo::EditIntent;
use gpui::{
    AppContext, Context, Entity, EntityInputHandler, IntoElement, Keystroke, Modifiers,
    MouseButton, Render, TestAppContext, VisualTestContext, Window, point, px, size,
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

#[test]
fn buffer_max_line_units_multiset_survives_multi_line_undo_redo() {
    // 行宽直方图（multiset）增量维护的正确性：跨多行替换 + 撤销/重做后
    // `max_line_units` 始终精确（O(1) 查询，无全量重扫）。
    let mut b = Buffer::new("aaa\nbb\nccc"); // 行宽 3 / 2 / 3
    assert_eq!(b.max_line_units(), 3, "有 tie 时 max=3");
    // 跨行替换：range 2..7 = "a\nbb\n"，换成 "XY" → "aaXYccc"（单行，宽 7）
    let e = b.apply_edit(2..7, "XY");
    assert_eq!(b.to_string(), "aaXYccc");
    assert_eq!(b.max_line_units(), 7, "跨行替换后精确");
    // 撤销 → 回到 "aaa\nbb\nccc"（max 3）
    b.undo_edit(&e);
    assert_eq!(b.max_line_units(), 3, "撤销后精确");
    // 重做 → "aaXYccc"（max 7）
    b.redo_edit(&e);
    assert_eq!(b.max_line_units(), 7, "重做后精确");
    // 再插一行更宽行 → max 增长（"超宽的尾行字" = 6 全角 = 12 格）
    b.apply_edit(b.len()..b.len(), "\n超宽的尾行字");
    assert_eq!(b.max_line_units(), 2 * 6, "追加的全角行 = 12 格");
    // 删除它 → 精确缩回 7
    let prefix = "aaXYccc\n".to_string();
    b.apply_edit(prefix.len()..prefix.len() + "超宽的尾行字".len(), "");
    assert_eq!(b.max_line_units(), 7, "删除后精确缩回");
}

#[test]
fn buffer_max_line_chars_initial_and_grows_on_edit() {
    // 横向内容宽度依据：全文最长行的**字符数**，须稳定 = 真实最长行。
    let mut b = Buffer::new("short\n这是最长的一行here\nend");
    // "这是最长的一行here" = 8 个汉字 + 4 个 ASCII = 12 字符? 数一下：
    // 这(1)是(2)最(3)长(4)的(5)一(6)行(7)h(8)e(9)r(10)e(11) → 11
    assert_eq!(b.max_line_chars(), 11);
    // 纯 ASCII 文件
    assert_eq!(Buffer::new("a\nbb\nccc").max_line_chars(), 3);
    // 空文档
    assert_eq!(Buffer::new("").max_line_chars(), 0);
    // 编辑写入更长的行 → 应更新
    b.apply_edit(b.len()..b.len(), "\n012345678901234567890"); // 21 字符新行
    assert_eq!(b.max_line_chars(), 21);
    // 编辑不产生更长行 → 保持不变
    b.apply_edit(0..0, "x"); // 首行变 xshort，仍短
    assert_eq!(b.max_line_chars(), 21);
    // 删除最长行 → 只增不减语义下保持旧值（横向过度留白，不做 O(n) 重扫）
    let prefix = "xshort\n这是最长的一行here\nend\n".to_string();
    let e = b.apply_edit(prefix.len()..prefix.len() + "012345678901234567890".len(), "");
    assert_eq!(b.max_line_chars(), 21, "删除最长行后不重扫，保持旧上界");
    // 撤销应能找回该行（仍 21）
    b.undo_edit(&e);
    assert_eq!(b.max_line_chars(), 21);
}

#[test]
fn buffer_sample_line_stats_placeholder_estimate() {
    // 大文档「先瞬时显示」占位横向量程：均匀采样 × 安全系数，真统计到达前横向
    // 滚动条不至于坍缩成视口宽。要求：恒采样第 0 行；估计 ≥ 采样最宽（×1.2 取整）；
    // 全空行返回空直方图（无内容 → 无横向量程）。
    use crate::editor::buffer::{sample_line_stats, LINE_SAMPLE_SAFETY};
    use ropey::Rope;

    // 最宽行恰在第 0 行（恒采样）：8 格 / 8 字符
    let rope = Rope::from("zzzzzzzz\nxy\nhi\n");
    let (mx_chars, hist) = sample_line_stats(&rope);
    assert_eq!(mx_chars, 8);
    let est = *hist.keys().next_back().expect("有估计值");
    assert_eq!(est, ((8.0 * LINE_SAMPLE_SAFETY).ceil()) as usize);
    assert!(est >= 8, "估计不得低估采样最宽");

    // 经 Buffer::from_parts 装配后，max_line_units 即返回该占位估计（滚动条消费方）
    let b = Buffer::from_parts(rope, mx_chars, hist);
    assert!(b.max_line_units() >= 8);

    // 采样值 0（全空行）→ 空直方图（保持「无内容 → 无横向量程」语义）
    let empty = Rope::from("\n\n\n\n");
    let (mx0, h0) = sample_line_stats(&empty);
    assert_eq!(mx0, 0);
    assert!(h0.is_empty());

    // 最宽行落在采样点之外的普通场景：估计只由采样值放大，安全系数保证 ≥ 采样最宽
    let mut src = String::from("aaa\n");
    for _ in 0..(crate::editor::buffer::LINE_SAMPLE_STRIDE * 2) {
        src.push_str("x\n");
    }
    let wide_miss = Rope::from(src.as_str());
    let (_, hmm) = sample_line_stats(&wide_miss);
    let est_miss = *hmm.keys().next_back().unwrap();
    assert!(
        est_miss >= 3 && est_miss >= ((3.0 * LINE_SAMPLE_SAFETY).ceil()) as usize,
        "采样估计应覆盖采样最宽：est={est_miss}"
    );
}

#[test]
fn buffer_max_line_units_counts_fullwidth() {
    // 宽度单位是「不折行」的依据（CJK 全角 = 2 格、ASCII = 1 格、Tab = 4 格），
    // 且可随删除缩小。任何行的真实字形宽 ≤ 单位 × 等宽格宽，StyledText 才不会折行。
    assert_eq!(Buffer::new("a\nbb\nccc").max_line_units(), 3);
    assert_eq!(Buffer::new("").max_line_units(), 0);
    assert_eq!(Buffer::new("是").max_line_units(), 2, "单个全角字 = 2 格");
    assert_eq!(Buffer::new("abcd中文").max_line_units(), 4 + 2 * 2);
    assert_eq!(Buffer::new("a\tb").max_line_units(), 1 + 4 + 1, "Tab = 4 格");
    let mut b = Buffer::new("short\n这是最长行here\nend");
    // 这(2)是(2)最(2)长(2)行(2)h(1)e(1)r(1)e(1) → 2*5 + 4 = 14
    assert_eq!(b.max_line_units(), 14);
    // 编辑写入更宽行 → 增长
    b.apply_edit(b.len()..b.len(), "\n中文很宽的一行字");
    assert_eq!(b.max_line_units(), 2 * 8, "8 个全角字 = 16 格");
    // 删除最宽行 → 行宽直方图（multiset）增量精确：量程立即缩到真实次宽行
    let prefix = "short\n这是最长行here\nend\n".to_string();
    b.apply_edit(prefix.len()..prefix.len() + "中文很宽的一行字".len(), "");
    assert_eq!(b.max_line_units(), 14, "删除最宽行后量程立即精确收敛到 14");
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

/// I1 时间窗合并：连续快速键入 → 一个撤销单元；停顿 >300ms 后继续键入 → 新起单元。
#[test]
fn undo_i1_breaks_group_after_300ms_pause() {
    let mut buf = Buffer::default();
    let mut mgr = UndoManager::new();
    // 连续快速键入 abc → 一个撤销单元。
    for (i, ch) in "abc".chars().enumerate() {
        mgr.start_tx(EditIntent::Typing, (i, i));
        mgr.push_edit(buf.apply_edit(i..i, &ch.to_string()));
        mgr.set_cooperative(true);
    }
    assert_eq!(buf.to_string(), "abc");
    // 停顿 >300ms 再键入 def（真实单调钟，远超 UNDO_GROUP_MS）。
    std::thread::sleep(std::time::Duration::from_millis(350));
    for (i, ch) in "def".chars().enumerate() {
        let at = 3 + i;
        mgr.start_tx(EditIntent::Typing, (at, at));
        mgr.push_edit(buf.apply_edit(at..at, &ch.to_string()));
        mgr.set_cooperative(true);
    }
    assert_eq!(buf.to_string(), "abcdef");
    // 第一次撤销只回退 def（停顿后新起单元），before 从停顿后的光标起。
    let tx = mgr.undo().expect("事务");
    assert_eq!(tx.edits.len(), 1);
    assert_eq!(tx.before, (3, 3));
    for e in tx.edits.iter().rev() {
        buf.undo_edit(e);
    }
    assert_eq!(buf.to_string(), "abc");
    // 第二次撤销回退 abc（仍是一整个单元）。
    let tx = mgr.undo().expect("事务");
    assert_eq!(tx.edits.len(), 1);
    for e in tx.edits.iter().rev() {
        buf.undo_edit(e);
    }
    assert_eq!(buf.to_string(), "");
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

/// 回归（已核实根因）：无语法（`syntax == None`，.txt 路径）时 `full_reparse` 只填
/// `lines` 空样式、`saves` 保持空 → 之后深跳触发容量收缩 `saves.drain(..drop)` 越界
/// panic。修复：None 分支逐行同步 push 纯文本解析器占位保存点恢复
/// `saves.len() == lines.len()` 不变量；`prepare_window` 收缩再加 debug 断言 + `drop`
/// 按 `saves.len()` 钳位双保险。
#[test]
fn no_syntax_deep_jump_shrink_keeps_saves_in_sync() {
    // 大文档（深跳行数足够触发容量收缩）：顶部小窗预热（空缓存 + lo==0 → None 分支
    // 只填 lines 空样式），再一次性深跳到文件深处 → 缓存上界远超请求窗口 → 收缩 drain。
    let text: String = (0..200_000)
        .map(|i| format!("no_syntax line {i:010}\n"))
        .collect();
    let b = Buffer::new(&text);

    let mut hl = Highlighter::new(); // 默认 syntax = None（无语法 / .txt 路径）
    // 顶部预热（空缓存 + lo==0 → full_reparse None 分支）。
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(top.len(), 109, "预热窗口应与请求对齐");
    assert!(top.iter().all(|r| r.is_empty()), "无语法行应全部为空样式");

    // 深跳：一次性大步下移 → 容量收缩（base 前移 + 丢弃顶部旧行）。
    let lo = 199_000usize;
    let hi = lo + 108;
    let rows = hl.highlight_window(&b, lo, hi, 16).to_vec();
    assert_eq!(rows.len(), hi - lo + 1, "深跳返回切片应与请求窗口对齐");
    assert!(rows.iter().all(|r| r.is_empty()), "深跳后无语法行仍全空样式");
    // 关键断言：收缩后 lines/saves 仍同步（修复前这里 saves.drain 越界 panic）。
    assert_eq!(
        hl.test_cache_len(),
        hl.test_saves_len(),
        "容量收缩后 lines/saves 必须保持同步"
    );
    assert!(
        hl.test_cache_len() > 0 && hl.test_cache_len() <= (hi - lo + 1) + 16,
        "深跳重锚应是有界窗口（非整文档），实际 {}",
        hl.test_cache_len()
    );

    // 深锚点继续向下小幅滚动 → extend_down/重锚路径，不 panic、切片仍对齐、不变量保持。
    let more = hl.highlight_window(&b, lo + 30, hi + 30, 16).to_vec();
    assert_eq!(more.len(), 109, "二次滚动窗口应仍对齐");
    assert_eq!(
        hl.test_cache_len(),
        hl.test_saves_len(),
        "多次滚动后 lines/saves 仍应同步"
    );
}

#[test]
fn far_down_jump_reanchors_bounded_not_full_prefix() {
    // 回归：大文档 `.rs` 拖滚动条时滑块极短，1 鼠标像素 ≈ 数千行，缺口（目标窗口底 −
    // 缓存上界）巨大。修复前 `extend_down` 会把整个缺口逐行重跑 syntect（16 万行级 →
    // 数百毫秒），UI 线程卡死 → 内容只有松开才动。修复后缺口超过单帧同步预算
    // （`EXTEND_SYNC_BUDGET_ROWS`，见 highlight.rs）改走**有界重锚**（O(窗口+回看)
    // ≈ 百行级）。测试用 wall-clock 兜底 + 切片/锚点断言：
    // 重锚后 base = lo − lookback、返回切片仍与请求窗口对齐。
    let text: String = (0..120_000)
        .map(|i| match i % 6 {
            0 => format!("fn foo{i}() {{ // 注释\n"),
            1 => format!("let x: u32 = {i} + 1; // tail\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            3 => format!("struct S{i} {{ a: u32, }}\n"),
            4 => format!("impl S{i} {{ fn m(&self) -> &Self {{ self }} }}\n"),
            _ => format!("let s = \"str{i}\"; /* block comment {i} */\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    // 预热（不计入计时，真实 App 打开/首滚时已发生）：全局语法集懒加载、ropey 大文行索引、顶部缓存。
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(top.len(), 109);
    let _deep_probe = b.line_text(72_000);

    // 纯 syntect 基线：另一次等价「有界重锚」的开销（~141 行新解析，Load 已预热）。
    // 作为相对阈值——debug/release 下 per-line syntect 成本差 ~30×，绝对 ms 阈值必然
    // 偏脆；只要远跳不超过基线 ~30×，就证明它没在逐行扫整个 7.2 万行缺口（那会是
    // ~500×+）。
    let base_us = {
        let t = std::time::Instant::now();
        hl.highlight_window(&b, 30_000, 30_124, 16).to_vec();
        t.elapsed().as_micros()
    };
    assert_eq!(hl.base(), 30_000 - 16);

    // 远跳到 ~60% 处（滚动条拖动）。
    let t = std::time::Instant::now();
    let rows = hl.highlight_window(&b, 72_000, 72_108, 16).to_vec();
    let far_us = t.elapsed().as_micros();
    assert_eq!(rows.len(), 72_108 - 72_000 + 1, "返回切片应与请求窗口对齐");
    assert_eq!(hl.base(), 72_000 - 16, "应重锚到 lo − lookback，而非把缓存建到 0");
    assert!(
        far_us < base_us.saturating_mul(30),
        "远跳高亮应≈一次有界重锚（基线 {base_us}µs，实测 {far_us}µs）——疑似仍在逐行扫整个缺口"
    );

    // 重锚后小幅向下滚：缺口小 → 正常 `extend_down`（只解析新增 ~92 行，不整前缀）。
    // 相对基线（~141 行重锚）校验：该成本应是「缺口行数」级别而非前缀（~500×）。
    let t2 = std::time::Instant::now();
    let again = hl.highlight_window(&b, 72_100, 72_200, 16).to_vec();
    let hit_us = t2.elapsed().as_micros();
    assert_eq!(again.len(), 101);
    assert!(
        hit_us < base_us.saturating_mul(2),
        "重锚后小幅滚动应只支付缺口行（基线 {base_us}µs，实测 {hit_us}µs）"
    );
}

#[test]
fn deferred_scroll_skips_syntect_until_settled() {
    // 回归：大文档 `.rs` 拖滚动条时，滚动中的每一帧都不能同步做 syntect 有界重锚
    // （DEBUG 下 ~百 ms/次 → 帧率个位数 → 滑块/内容跟不紧鼠标）。`build_rows` 的
    // 快路径把窗口登记为 `pending`、本帧用空样式渲染；窗口稳定（相邻帧一致）后
    // `settle_pending` 补一次重锚上色。这里验证 Highlighter 的延后 API 契约：
    // 覆盖/小缺口 → 快路径；大缺口 → 延后；settle → 有颜色且对齐。
    let text: String = (0..120_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // 注释 {i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));

    // 首屏（P5：空缓存 + 大首窗 + 语法）→ **延后分片补色**——本帧不整窗同步解析
    // （先出纯文本、颜色逐帧从可见顶补，debug 整窗 ~500ms 冻结 → 首切片 ~50ms），
    // 亦不置 pending（避免下一帧 settle 整窗重锚把首帧冻结打回来）。直接整窗请求
    // 等价于补色完成后的 API（编辑器 `syntect_fill_deferred` 逐 32 行调用）。
    assert!(!hl.accept_deferred(0, 108, 16), "大首窗应延后分片补色（不再同步整窗解析）");
    assert_eq!(hl.pending(), None, "延后首窗不登记 pending");
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert!(!top[0].is_empty(), "窗口从可见顶起有颜色");
    // 缓存就绪后，同窗口再次请求命中快速路径（覆盖 → 不登记 pending）。
    assert!(hl.accept_deferred(0, 108, 16), "缓存命中应走快速路径");
    assert_eq!(hl.pending(), None, "快路径不登记 pending");

    // 大缺口：注册 pending，且**不**立刻重锚（本帧空样式渲染的前提）。
    let quick = hl.accept_deferred(72_000, 72_108, 16);
    assert!(!quick, "大缺口应延后（本帧跳过 syntect）");
    assert_eq!(hl.pending(), Some((72_000, 72_108, 16)));

    // 下一帧同一窗口 → 滚动已停 → settle 补重锚 → 之后有颜色且切片对齐。
    let prev = hl.pending();
    let quick2 = hl.accept_deferred(72_000, 72_108, 16);
    assert!(!quick2);
    assert_eq!(prev, Some((72_000, 72_108, 16)), "相邻帧窗口一致 → 满足 settle 前提");
    assert!(hl.settle_pending(&b), "settle 应补齐缓存");
    let rows = hl.highlight_window(&b, 72_000, 72_108, 16).to_vec();
    assert_eq!(rows.len(), 109);
    assert!(!rows[0].is_empty(), "settle 后窗口应有颜色");
    assert_eq!(hl.pending(), None, "settle 消耗掉 pending");

    // 滚动停住后（缓存深锚在 71984）切回顶部小窗：上移越出缓存同样要重锚 → 一致延后
    //（不引入「上滚一键瞬卡」）。窗口稳定后 settle 同样能补齐。
    assert!(!hl.accept_deferred(0, 60, 16), "上移出缓存应同样延后");
    assert_eq!(hl.pending(), Some((0, 60, 16)));
    let prev2 = hl.pending();
    assert!(!hl.accept_deferred(0, 60, 16));
    assert_eq!(prev2, Some((0, 60, 16)));
    assert!(hl.settle_pending(&b), "上移的 settle 也应补齐缓存");
    assert_eq!(hl.highlight_window(&b, 0, 60, 16).len(), 61);
}

#[test]
fn medium_gap_scroll_defers_until_settled() {
    // I3 回归：中速拖动/惯性滚「缺口 ~500 行」——修复前缺口 < 旧 `REANCHOR_GAP`(1024)
    // 走 `extend_down` 每帧同步逐行重跑整个缺口（~63µs/行，~500 行 ≈ 40-100ms/跳
    // release / 百 ms 级 debug → 滑块跟不上鼠标）；修复后缺口超过 `EXTEND_SYNC_BUDGET_ROWS`
    // 一律延后（登记 pending、本帧跳过 syntect、空样式渲染），静止 settle 一次**有界
    // 重锚**（O(窗口+回看)≈一屏）上色——与既有大缺口路径共用同一 pending/settle 机制。
    // 此测试锁定「~500 行缺口（100-900 行中速带）」也走延后，不再吃旧的同步扩展行为。
    let text: String = (0..2_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    // 首屏（P5：大首窗延后分片补色；直接整窗请求等价于补色完成后的 API）。
    assert!(!hl.accept_deferred(0, 108, 16), "大首窗应延后分片补色");
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert!(!top[0].is_empty(), "首屏顶部应有颜色");

    // 中速大缺口（~500 行，落在 100-900 中速带、且 < 旧 1024 阈值）：应延后而非同步扩展。
    let (lo, hi) = (500usize, 608usize);
    assert!(
        !hl.accept_deferred(lo, hi, 16),
        "缺口 500 行（> 单帧预算）应走延后，不再每帧同步逐行扩展"
    );
    assert_eq!(hl.pending(), Some((lo, hi, 16)), "延后应登记 pending 供 settle");
    // 延后帧用空样式渲染：settle 前窗口尚未重锚（base 还停在顶部锚点）。
    assert_eq!(hl.base(), 0, "settle 前不得提前重锚（本帧空样式渲染的前提）");

    // 相邻帧窗口一致（滚动已停）→ settle 一次有界重锚即补齐，且只付一屏解析。
    assert!(!hl.accept_deferred(lo, hi, 16), "次帧同一窗口仍延后（满足 settle 前提）");
    assert_eq!(hl.pending(), Some((lo, hi, 16)), "相邻帧窗口一致 → 满足 settle 前提");
    assert!(hl.settle_pending(&b), "settle 应补齐缓存");
    assert_eq!(hl.base(), lo - 16, "settle 有界重锚到 lo − lookback，而非逐行扫缺口");
    let rows = hl.highlight_window(&b, lo, hi, 16).to_vec();
    assert_eq!(rows.len(), hi - lo + 1, "settle 后切片与请求窗口对齐");
    assert!(!rows[0].is_empty(), "settle 后窗口应有颜色");
    assert_eq!(hl.pending(), None, "settle 消耗 pending");
}

#[test]
fn small_gap_scroll_stays_sync_path() {
    // I3 护栏：缺口 ≤ `EXTEND_SYNC_BUDGET_ROWS`（滚轮/惯性微跳，每帧几~几十行）维持
    // 现状**同步扩展**——`accept_deferred` 返回快路径、不登记 pending、颜色即时，
    // 避免每帧都延后一帧的延迟；且扩展保持 `base` 不动（作用域栈连续，非重锚）。
    let text: String = (0..2_000)
        .map(|i| format!("let x: u32 = {i}; // c{i}\n"))
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    // 首屏（P5：大首窗延后分片补色；直接整窗请求等价于补色完成后的 API）。无预热 →
    // 缓存仅止于请求窗 [0,108]（不再是旧 160 行预热后的 [0,~268]）。
    assert!(!hl.accept_deferred(0, 108, 16), "大首窗应延后分片补色");
    let _ = hl.highlight_window(&b, 0, 108, 16).to_vec();
    // 把缓存前缀扩到 268（gap 160 ≤ 预算 → 同步扩展，等价于补色自尊完成后的一次滚动）。
    assert!(hl.accept_deferred(160, 268, 16), "≤预算缺口应同步扩展");
    let _ = hl.highlight_window(&b, 160, 268, 16).to_vec();

    // 小缺口下滚：窗口 [260,380] 越出缓存上界 112 行（≤ 预算）→ 同步扩展、颜色即时。
    let (lo, hi) = (260usize, 380usize);
    assert!(hl.accept_deferred(lo, hi, 16), "小缺口应走同步快路径（不延后）");
    assert_eq!(hl.pending(), None, "同步路径不登记 pending");
    assert_eq!(hl.base(), 0, "同步扩展保持 base 不动（作用域栈连续）");
    let rows = hl.highlight_window(&b, lo, hi, 16).to_vec();
    assert_eq!(rows.len(), hi - lo + 1, "切片与请求窗口对齐");
    assert!(!rows[0].is_empty(), "同步扩展立即有颜色");
}

/// 回跳快照 LRU 半程续栈回归（主需求）：打开 → 远跳下滚 → 回看顶部 → 再回跳下滚。
///
/// - **修复前**：回看（`lo < base`）落到整段重解析——每次回跳 `full_reparse` +1
///   （本测试「打开→下滚→回看」中回看那次会从 2 → 3，断言 `== 2` 修复前必失败）；
/// - **修复后**：离开顶部窗口前（drain / 重锚）先沉降为快照，回看命中快照从中重建
///   （零 syntect），且回跳时也把「当前窗口」沉降——往返再下滚同样命中快照，不再
///   整段重解析。计数探针：`test_reparse_count`（整窗重解析）vs `test_snapshot_hit_count`
///   （回跳命中快照半程续栈）。
#[test]
fn jump_back_hits_snapshot_no_full_reparse() {
    let text: String = (0..20_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));

    // 打开顶部窗口：整段重解析 #1（离开前的沉降在首次无窗可沉 → 快照为空）。
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(hl.test_reparse_count(), 1, "首窗建缓存恰一次整段重解析");
    assert_eq!(hl.test_snapshot_hit_count(), 0);

    // 远跳下滚到 [10000,10108]：容量自愈整窗 drain → 顶窗先沉降为快照 → 整段重解析 #2。
    let rows2 = hl.highlight_window(&b, 10_000, 10_108, 16).to_vec();
    assert_eq!(rows2.len(), 109);
    assert_eq!(hl.test_reparse_count(), 2, "下滚远跳仍整段重解析（一次）");
    assert_eq!(hl.test_snapshot_hit_count(), 0, "下滚无快照可命中");

    // 回看顶部 [0,108]：命中刚才沉降的快照 → 不再整段重解析；颜色与首窗冷解析逐位一致。
    let back = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(back.len(), 109);
    assert_eq!(
        hl.test_reparse_count(),
        2,
        "回看应命中快照半程续栈，不再整段重解析（修复前这里 = 3）"
    );
    assert_eq!(hl.test_snapshot_hit_count(), 1, "回看恰命中一次快照半程续栈");
    assert_eq!(back, top, "回看（快照半程续栈）颜色应与首窗冷重解析逐位一致");

    // 再回跳下滚到 [10000,10108]：回看时沉降的「旧主窗口」仍在 LRU → 再命中，仍零整段重解析。
    let again = hl.highlight_window(&b, 10_000, 10_108, 16).to_vec();
    assert_eq!(again.len(), 109);
    assert_eq!(hl.test_reparse_count(), 2, "往返再跳也应命中快照，不整段重解析");
    assert_eq!(hl.test_snapshot_hit_count(), 2, "第二次也命中快照半程续栈");
    assert_eq!(again, rows2, "往返颜色应与首次冷解析逐位一致");
}

/// 快照半程续栈的**续解析正确性**：回跳窗口横跨快照上界（快照只覆盖前缀、窗口底伸到
/// 快照之下）时，从快照最后保存点续解析——输出必须与「整窗冷重解析（全新 Highlighter）」
/// 逐位一致。
#[test]
fn jump_back_resume_extends_beyond_snapshot_matches_cold() {
    let text: String = (0..5_000)
        .map(|i| match i % 4 {
            0 => format!("fn a{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\"); /* b{i} */\n"),
            _ => format!("struct B{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));

    // 上小窗 [0,20]（预热建 [0,~62] 缓存）→ 远跳 [400,450]（顶窗沉降为快照）。
    let _ = hl.highlight_window(&b, 0, 20, 16).to_vec();
    let _ = hl.highlight_window(&b, 400, 450, 16).to_vec();
    assert_eq!(hl.test_reparse_count(), 2);

    // 回跳窗口 [0,300]：快照只盖 [0,62]，窗口底（300）在快照之下 → 从快照最后保存点
    // 续解析 63..=300。冷重解析对照（全新 Highlighter 同样从行 0 起锚）必须逐位一致。
    let merged = hl.highlight_window(&b, 0, 300, 16).to_vec();
    assert_eq!(merged.len(), 301);
    assert_eq!(hl.test_reparse_count(), 2, "续栈不触发整段重解析");
    assert_eq!(hl.test_snapshot_hit_count(), 1);

    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 300, 16).to_vec();
    assert_eq!(merged, truth_rows, "快照续栈输出应与冷整段重解析逐位一致");
}

/// 编辑 → 全部快照失效：内容变更后，回跳/重锚不得命中任何旧快照（否则把旧颜色当新
/// 文档用）。编辑本身由既有编辑增量路径（`reparse_after_edit`）处理、不触发整段重解析；
/// 但之后的大缺口下滚再重锚时，被编辑清空的快照不能再命中 → 必须整段重解析（修复前若
/// 不失效会命中旧快照）。
#[test]
fn edit_clears_snapshots_next_reanchor_full_reparse() {
    let text: String = (0..20_000)
        .map(|i| format!("let v{i}: u32 = {i}; // c{i}\n"))
        .collect();
    let mut b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));

    // 打开 → 远跳下滚（快照沉降）→ 回看（命中快照）。至此快照里有一颗 [10000,10108]。
    let _ = hl.highlight_window(&b, 0, 108, 16).to_vec();
    let _ = hl.highlight_window(&b, 10_000, 10_108, 16).to_vec();
    let _ = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(hl.test_reparse_count(), 2);
    assert_eq!(hl.test_snapshot_hit_count(), 1, "编辑前回看已命中快照");

    // 编辑行 50 前插入一行（version 0→1），登记编辑 → 快照必须全部失效。
    let off = b.line_start(50);
    b.apply_edit(off..off, "X\n");
    hl.set_dirty(Some((50, 51, 1)));

    // 编辑增量路径就地改色（不整段重解析；改后颜色 = 全新解析）。
    let after: Vec<_> = hl.highlight_window(&b, 0, 108, 16).to_vec();
    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 108, 16).to_vec();
    assert_eq!(hl.test_reparse_count(), 2, "编辑增量不应触发整段重解析");
    assert_eq!(after, truth_rows, "编辑后窗口颜色应与全新解析一致");

    // 再下滚 [10000,10108]：编辑清空了快照 → 命不中旧快照 → 必须整段重解析 #3。
    let _ = hl.highlight_window(&b, 10_000, 10_108, 16).to_vec();
    assert_eq!(
        hl.test_reparse_count(),
        3,
        "编辑后回跳不得命中旧快照，须整段重解析"
    );
    assert_eq!(
        hl.test_snapshot_hit_count(),
        1,
        "编辑后不得再命中快照（快照已失效）"
    );
}

#[test]
fn highlight_window_aligned_to_base_for_batched_indexing() {
    // 视图 `build_rows` 依赖 `highlight_window(lo, hi, lookback)` 返回**与请求
    // 窗口对齐**的切片：第 0 项 = `lo` 行、总长 = hi − lo + 1，可用 `rows[r - lo]`
    // 直接按行取。缓存命中（返回子切片）与未命中（重解析）两条路径都必须一致，
    // 否则滚动窗口的高亮会错位/越界。
    let src = (0..50)
        .map(|i| format!("line{i} fun() // c{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let b = Buffer::new(&src);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let lo = 20usize;
    let hi = 25usize;
    let rows = hl.highlight_window(&b, lo, hi, 16).to_vec();
    // `base()` 保留内部缓存起点（含回看），但返回切片对齐到 lo：
    assert_eq!(hl.base(), lo - 16, "内部缓存起点仅用于命中判定，不直接暴露给调用方");
    assert_eq!(
        rows.len(),
        hi - lo + 1,
        "返回切片应与请求窗口同行数（第 0 项 = lo 行）"
    );
    for r in lo..=hi {
        let runs = &rows[r - lo];
        assert!(!runs.is_empty(), "行 {r} 应有高亮段（r−lo={}）", r - lo);
    }
    // 缓存命中（同窗口再查）应返回等长、内容一致的 lo 对齐切片
    let rows2 = hl.highlight_window(&b, lo, hi, 16).to_vec();
    assert_eq!(rows2.len(), hi - lo + 1);
    assert_eq!(&rows2[0], &rows[0], "缓存命中，内容稳定");
    // 相邻窗口（滚动 1 行）在缓存内也应得 lo 对齐切片
    let rows3 = hl.highlight_window(&b, lo + 1, hi + 1, 16).to_vec();
    assert_eq!(rows3.len(), hi + 1 - (lo + 1) + 1);
    for r in (lo + 1)..=(hi + 1) {
        let runs = &rows3[r - (lo + 1)];
        assert!(!runs.is_empty(), "滚动窗口行 {r} 应有高亮段");
    }
}

/// 辅助：全量重解析（独立 Highlighter）+ set_dirty 增量路径 对照同一 buffer 的
/// `[lo, hi]` 高亮，断言完全一致（增量正确性金标准）。
fn assert_incremental_matches_full(src: &str, edit: impl FnOnce(&mut Buffer) -> (usize, usize, i64)) {
    let mut b = Buffer::new(src);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let hay: Vec<_> = hl.highlight_window(&b, 0, 40, 16).to_vec();
    assert!(!hay.is_empty(), "暖缓存应有产出");

    let (min_row, tail_new, delta) = edit(&mut b);
    hl.set_dirty(Some((min_row, tail_new, delta)));
    let after: Vec<_> = hl.highlight_window(&b, 0, 40, 16).to_vec();

    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 40, 16).to_vec();
    assert_eq!(
        after, truth_rows,
        "增量高亮应与全量重解析一致（min_row={min_row} tail_new={tail_new} delta={delta}）"
    );
}

/// 纯行内编辑（无换行）：只有编辑行的局部重解析，且输出与全量一致。
#[test]
fn incremental_edit_inline_matches_full() {
    // 结尾**不带**换行（避免尾幻影空行——在尾行行首编辑会误当「新行替换幻影」，
    // 覆盖真实「行内编辑」场景的干净样本）。
    let src = "fn main() {\n    let a = 1; // 注释\n    let s = \"str\";\n    call();\n}";
    for k in 0..src.lines().count() {
        let s = src.to_string();
        let want = |b: &mut Buffer| -> (usize, usize, i64) {
            let line = k.min(b.line_count().saturating_sub(1));
            let start = b.line_start(line);
            b.apply_edit(start..start, "XYZ");
            (line, line + 1, 0)
        };
        assert_incremental_matches_full(&s, want);
    }
}

/// 行内插入多处重复（连续打字合并场景）仍与全量一致。
#[test]
fn incremental_edit_repeated_inline_matches_full() {
    let src = "let q = \"abc\";\nlet r = 42; // x\nfn f() {}\n";
    assert_incremental_matches_full(src, |b| {
        let l = 1;
        let start = b.line_start(l);
        for _ in 0..3 {
            b.apply_edit(start..start, "注入");
        }
        (l, l + 1, 0)
    });
}

/// 换行插入（行数 +1，下方整体位移）：增量必须逐位移对照、与全量一致。
#[test]
fn incremental_edit_insert_newline_matches_full() {
    let src = "aa\nbb\ncc\ndd\nee\n// tail\n";
    assert_incremental_matches_full(src, |b| {
        let l = 2; // 在第 3 行（cc）行尾插入两段换行 → 3 行新内容
        let pos = b.line_end(l); // 行尾字节偏移（不含换行符）→ 在旧 `\n` 前插入
        b.apply_edit(pos..pos, "\n#{mid}\n");
        (l, l + 3, 2)
    });
}

/// 删除换行（行数 -1，下方整体上移）：增量必须逐位移对照、与全量一致。
#[test]
fn incremental_edit_delete_newline_matches_full() {
    let src = "aa\nbb\ncc\ndd\nee\n// tail\n";
    assert_incremental_matches_full(src, |b| {
        let l = 2;
        // 删除第 3 行行尾换行（line_end 不含换行符 → [line_end, line_end+1) 即 `\n`）
        let pos = b.line_end(l);
        b.apply_edit(pos..pos + 1, "");
        (l, l + 1, -1)
    });
}

/// 编辑**开启了新的多行作用域**（块注释）：下方着色会变，增量必须继续解析到
/// 作用域闭合，绝不提前在「样式巧合一致」处收敛（防过度收敛回归）。
#[test]
fn incremental_edit_opens_scope_propagates_down() {
    let src = "let a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\nlet e = 5;\nlet f = 6;\nlet g = 7;\nlet h = 8;\nlet i = 9;\nlet j = 10;\n";
    assert_incremental_matches_full(src, |b| {
        let l = 1;
        let start = b.line_start(l);
        // 第 2 行插入未闭合块注释开头：第 2 行起下方全部变注释色。
        b.apply_edit(start..start, "/*");
        (l, l + 1, 0)
    });
}

/// 编辑处于既有多行块注释内部（下方仍在同一作用域 → 状态不变，可合法提前收敛）：
/// 增量结果仍与全量一致。
#[test]
fn incremental_edit_inside_multiline_comment_matches_full() {
    let src = "/* 多行注释头部\n第二行内容 abc\n第三行内容 def\n第四行 */\nlet after = 1;\n";
    assert_incremental_matches_full(src, |b| {
        let l = 1;
        let start = b.line_start(l) + 3;
        b.apply_edit(start..start, "XYZ");
        (l, l + 1, 0)
    });
}

/// P4-3 回归：**文首编辑（min_row == base == 0）**应走增量快速路径（新鲜 `ParseState`
/// 从第 0 行起、收敛早退），且增量结果必须与整段重解析完全一致——含顶部多行作用域
/// 未闭合时向下方传播的着色。修复前 `prepare_window` 对 `min_row == base` 一律回退整窗
/// 重解析（.rs 大文档窗口 512 行 × 每键 ~90ms debug）；修复后文首编辑以 base==0 的新鲜
/// 状态增量重解析（楼上作用域不存在，无丢失风险）。
#[test]
fn incremental_edit_at_row_zero_matches_full() {
    // 顶部行内编辑（常见打字）：应快速收敛且与全量一致。
    let src = "let top = 1; // x\nlet a = 2;\nlet b = 3;\nfn g() {}\n";
    assert_incremental_matches_full(src, |b| {
        let start = b.line_start(0);
        b.apply_edit(start..start, "XYZ");
        (0, 1, 0)
    });
    // 顶部插入未闭合块注释：从第 0 行起开启多行作用域，下方着色必须全部传播
    // （新鲜状态路径不得丢失块注释作用域）。
    let src2 = "fn f() {\n    let a = 1;\n    let b = 2;\n}\nfn g() {}\n";
    assert_incremental_matches_full(src2, |b| {
        let start = b.line_start(0);
        b.apply_edit(start..start, "/*");
        (0, 1, 0)
    });
}

/// 撤回再重写同一行（编辑行视觉输出回到旧样子）：增量在编辑行本身即收敛，
/// 下方复用旧缓存，仍与全量一致。
#[test]
fn incremental_edit_restores_same_style_converges_at_edit_row() {
    let src = "let v = \"str\";\nlet w = 42;\nlet z = 1;\n";
    assert_incremental_matches_full(src, |b| {
        let l = 1;
        let start = b.line_start(l);
        b.apply_edit(start..start, "X"); // 行 1 变为 string
        (l, l + 1, 0)
    });
}

/// 回归：编辑 + 同帧请求窗口下移超出旧缓存上界 —— 增量收敛后尾部按位移复制必须
/// 做界校验（超出旧缓存时回落整段重解析），否则 `old_lines[or - old_base]` 越界 panic。
#[test]
fn incremental_edit_grows_window_beyond_cache_matches_full() {
    let src = (0..80)
        .map(|i| format!("line{i} // c{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut b = Buffer::new(&src);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    // 暖缓存：小窗口 [0,20]（首屏预热后缓存覆盖 0..=62）。
    let _ = hl.highlight_window(&b, 0, 20, 16).to_vec();
    // 编辑第 0 行行首插入换行（净行 +1）
    let pos = b.line_start(0);
    b.apply_edit(pos..pos, "\n");
    hl.set_dirty(Some((0, 2, 1)));
    // 请求窗口大幅下移 [0,80]：底（80）超出旧缓存上界（62）+ delta → 尾部位移行越出
    // 旧缓存，必须界校验后回落整段重解析（否则 `old_lines[or - old_base]` 越界 panic）。
    let after: Vec<_> = hl.highlight_window(&b, 0, 80, 16).to_vec();
    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 80, 16).to_vec();
    assert_eq!(
        after, truth_rows,
        "编辑+窗口下移超出旧缓存后应回落整段重解析，不得越界 panic"
    );
}

/// 回归：同一帧发生多次编辑（版本 +N）才高亮 —— 单槽 `hl_dirty` 只反映最后一次编辑
/// 的 delta/位移，不可信 → 必须回落整段重解析（否则位移映射失真可能错误收敛）。
#[test]
fn coalesced_edits_before_highlight_match_full() {
    let src = "aa\nbb\ncc\ndd\nee\nff\ngg\nhh\nii\njj\n";
    let mut b = Buffer::new(src);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let _ = hl.highlight_window(&b, 0, 12, 16).to_vec();
    // 编辑 1：第 2 行行尾插入换行（净行 +1）
    let pos1 = b.line_end(2);
    b.apply_edit(pos1..pos1, "\nX");
    // 编辑 2：第 4 行行首插入（同帧，覆盖 single-slot dirty）
    let pos2 = b.line_start(4);
    b.apply_edit(pos2..pos2, "Y");
    // 只做一次高亮（hl_dirty 只剩最后一次编辑的信息）
    hl.set_dirty(Some((4, 5, 0)));
    let after: Vec<_> = hl.highlight_window(&b, 0, 12, 16).to_vec();
    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 12, 16).to_vec();
    assert_eq!(
        after, truth_rows,
        "多连编辑后单次高亮应等于全量（path B 守卫 version==gated+1 → 回落整段重解析）"
    );
}

/// 回归：整篇替换文档（新 Buffer 同为 version 0）后必须整段重解析，不得命中上一份
/// 文档的缓存颜色（`Highlighter::invalidate` 防 version-0 冲突）。
#[test]
fn highlighter_invalidate_prevents_version_zero_collision() {
    let doc_a = "// only a comment\nlet x = 1;\n";
    let doc_b = "let s = \"str\" + \"ing\";\nlet n = 99;\n";
    let truth_b = {
        let mut truth = Highlighter::new();
        truth.set_language(Some("rust"));
        truth.highlight_window(&Buffer::new(doc_b), 0, 1, 16).to_vec()
    };
    // 修复路径：invalidate 后才切源 → 与全量一致（不残留 doc_a 颜色）。
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let _ = hl.highlight_window(&Buffer::new(doc_a), 0, 1, 16).to_vec();
    hl.invalidate();
    let b1: Vec<_> = hl.highlight_window(&Buffer::new(doc_b), 0, 1, 16).to_vec();
    assert_eq!(b1, truth_b, "invalidate 后新文档应与全量一致");
    // 对照：不 invalidate 时同 version0 会命中旧缓存（证明该修复必要）。
    let mut hl2 = Highlighter::new();
    hl2.set_language(Some("rust"));
    let a0: Vec<_> = hl2.highlight_window(&Buffer::new(doc_a), 0, 1, 16).to_vec();
    let stale: Vec<_> = hl2.highlight_window(&Buffer::new(doc_b), 0, 1, 16).to_vec();
    assert_eq!(stale, a0, "未 invalidate 时应残留旧文档颜色（version0 冲突路径）");
    assert_ne!(a0, truth_b, "doc_a 与 doc_b 首行颜色应不同（否则测试无意义）");
}

/// 复现 gpui `StyledText::compute_runs` 的长度求和逻辑，验证「选区叠在语法段上」
/// 是否把 run 总量撑爆（run 总长 > 文本长 → shape_text 按行尾截断 → 选区底色丢失）。
/// 这正是 .rs（Rust 每行多段高亮）无法鼠标选择、而 .txt/.md（同字节无高亮）正常的原因。
#[test]
fn selection_run_must_not_oversubscribe_row() {
    let text = "fn main() { // 注释";
    let b = Buffer::new(text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let rows = hl.highlight_window(&b, 0, 0, 0).to_vec();
    let runs = rows.first().cloned().unwrap_or_default();
    assert!(!runs.is_empty(), "rust 行应有语法段");

    // 选区整行选中（line-local 0..text.len()）
    let sel = 0..text.len();

    // —— gpui 路径（旧行为）：语法段 + 尾巴再附一个重叠选区段 ——
    let mut old: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::new();
    for (r, color) in &runs {
        let s = r.start.min(text.len());
        let e = r.end.min(text.len());
        if e > s {
            old.push((
                s..e,
                gpui::HighlightStyle {
                    color: Some(*color),
                    ..Default::default()
                },
            ));
        }
    }
    old.push((
        sel.clone(),
        gpui::HighlightStyle {
            background_color: Some(gpui::rgb(0x005fb8).into()),
            ..Default::default()
        },
    ));
    // 复刻 compute_runs：逐条 push「gap + highlight.len」
    let mut ix = 0usize;
    let mut subscribed = 0usize;
    for (range, _) in &old {
        if ix < range.start {
            subscribed += range.start - ix;
        }
        subscribed += range.len();
        ix = range.end;
    }
    assert!(
        subscribed > text.len(),
        "旧路径必须超订（run 总长 {subscribed} > 行字节长 {}），否则本测试前提不成立",
        text.len()
    );
}

/// 修复后：`merge_highlights` 把语法段 + 选区合并成非重叠段，run 总量不再超订，
/// 且选区范围整段都有选区底色（可被 gpui 画出）。
#[test]
fn merge_highlights_keeps_selection_within_line_and_non_overlapping() {
    use gpui::HighlightStyle;

    let text = "fn main() { // 注释";
    let b = Buffer::new(text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let rows = hl.highlight_window(&b, 0, 0, 0).to_vec();
    let runs = rows.first().cloned().unwrap_or_default();
    assert!(!runs.is_empty(), "rust 行应有语法段");
    // `merge_highlights` 现在消费「仅色」的 `HighlightStyle` 段（syntect `(Range, Hsla)`
    // 由 `build_rows` 转换；这里等价转换喂入）。
    let runs: Vec<(std::ops::Range<usize>, HighlightStyle)> = runs
        .iter()
        .map(|(r, c)| (r.clone(), HighlightStyle { color: Some(*c), ..Default::default() }))
        .collect();

    let sel_bg = gpui::rgb(0x005fb8).opacity(0.28).into();
    // 选区从中部到行尾（横跨多个语法段，正是旧的“重叠尾巴”场景）
    let sel = 6..text.len();
    let merged = crate::editor::view::merge_highlights(&runs, Some(sel.clone()), sel_bg);
    assert!(!merged.is_empty(), "合并后应有段");

    // 1) 非重叠且严格递增、都在行内
    let mut prev_end = 0usize;
    for (r, _) in &merged {
        assert!(r.start >= prev_end, "段应非重叠有序: {r:?}");
        assert!(r.end <= text.len(), "段应在行内: {r:?}");
        prev_end = r.end;
    }

    // 2) 复刻 gpui compute_runs 的总量：不得超订
    let mut ix = 0usize;
    let mut subscribed = 0usize;
    for (r, _) in &merged {
        if ix < r.start {
            subscribed += r.start - ix;
        }
        subscribed += r.len();
        ix = r.end;
    }
    assert!(
        subscribed <= text.len(),
        "合并后 run 总量 {subscribed} 不得超过行字节长 {}",
        text.len()
    );

    // 3) 选区整段都有背景色（语法色与背景色可并存）
    for (r, st) in &merged {
        if r.end <= sel.start || r.start >= sel.end {
            continue;
        }
        assert!(
            st.background_color == Some(sel_bg),
            "选区段 {r:?} 应带背景色; style={:?}",
            st
        );
    }
    let covered: usize = merged
        .iter()
        .filter(|(_, st)| st.background_color == Some(sel_bg))
        .map(|(r, _)| r.end.min(sel.end).saturating_sub(r.start.max(sel.start)))
        .sum();
    assert_eq!(
        covered,
        sel.end - sel.start,
        "选区范围每个字节都应有背景色（covered={covered}）"
    );

    // 4) 无选区时，仅语法段、总量恰等于语法覆盖（仍不超订）
    let plain = crate::editor::view::merge_highlights(&runs, None, sel_bg);
    assert!(!plain.is_empty(), "无选区也应有语法段");
}

/// 回归（用户确认）：`styled_row_text` 用 `StyledText::with_highlights` 渲染 `rd.highlights`，
/// gpui `elements/text.rs` 里对每个 run 都有 `is_char_boundary(start/end)` debug_assert
/// ——编辑→重析后行文本变短、或上游按字节搬运的区间切进 CJK/emoji 多字节字符内部时
/// （如「WYSIWYG 删除标题记号 `#` → 切换」令标题行变普通段落、区间端越过新行长或落在
/// 字符中），会在 dev 构建直接 panic。`char_align_highlights` 是 `styled_row_text` 的咽喉
/// 兜底：把陈旧/脏区间吸收到`text`字符边界、钳入行尾、空段丢弃、段间不重叠。
#[test]
fn char_align_highlights_survives_stale_ranges() {
    use gpui::{HighlightStyle, SharedString, StyledText};

    // "测🍎试❗"：测=3B，🍎=4B，试=3B，❗=3B → 13 字节。
    let text = "测🍎试❗";
    assert_eq!(text.len(), 13);

    let bold = HighlightStyle {
        font_weight: Some(gpui::FontWeight::BOLD),
        ..Default::default()
    };
    let italic = HighlightStyle {
        font_style: Some(gpui::FontStyle::Italic),
        ..Default::default()
    };

    // 一把“脏”区间：越界 end / start-end 都切在多字节字符内 / 反向区间。
    #[allow(clippy::reversed_empty_ranges)] // 故意喂「反向（空）」区间，检验钳位兜底不 panic
    let stale = vec![
        (1..20, bold),   // start 切进『测』(0..3) 内部，end 越界
        (8..8, italic),  // 空区间
        (4..8, bold),    // start=4、end=8 都不在字符边界
        (13..1, italic), // 反向区间
    ];

    let clamped = crate::editor::view::char_align_highlights(text, &stale);

    // 0) 机制证明：**不钳就直接喂** `StyledText::with_highlights`，坏区间会在 dev 构建
    //    触发 gpui `elements/text.rs` 的 `is_char_boundary(run.end)` debug_assert（用户
    //    报告的正是这条 panic 路径）——先确认机制存在，再证明钳后不再触发。
    let raw_bad = vec![(4..8, bold)];
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _st = StyledText::new(SharedString::from(text)).with_highlights(raw_bad.clone());
    }));
    assert!(
        panicked.is_err(),
        "未钳的坏区间 {raw_bad:?} 喂 StyledText 应在 dev 构建 panic（复刻用户断言）"
    );

    // 1) 每个段都在字符边界内、互不重叠递增、总量不超行字节长。
    let mut prev = 0usize;
    let mut covered = 0usize;
    for (r, _) in &clamped {
        assert!(r.start >= prev, "段应递增不重叠: {clamped:?}");
        assert!(r.end > r.start, "不应产出空段: {clamped:?}");
        assert!(r.end <= text.len(), "end {r:?} 越界 {}", text.len());
        assert!(
            text.is_char_boundary(r.start),
            "start {} 不是字符边界: {clamped:?}",
            r.start
        );
        assert!(
            text.is_char_boundary(r.end),
            "end {} 不是字符边界: {clamped:?}",
            r.end
        );
        prev = r.end;
        covered += r.end - r.start;
    }
    assert!(covered <= text.len(), "覆盖总量 {covered} > {}", text.len());

    // 2) 交给真实 `StyledText::with_highlights` 不 panic（走同一 debug_assert 咽喉）。
    let _st = StyledText::new(SharedString::from(text)).with_highlights(clamped.clone());

    // 3) 语义验证：越界+中切段应被吸收成整行加粗（[3..13) 覆盖全字），不丢样式。
    assert!(
        clamped.iter().any(|(r, s)| {
            r.start == 3 && r.end == 13 && s.font_weight == bold.font_weight
        }),
        "被吸收的区间应收敛成 [3..13) 加粗段: {clamped:?}"
    );

    // 4) 已对齐输入幂等：merging_highlights 的正常输出原样保留。
    let clean = vec![
        (0..3, bold),
        (3..7, italic), // 🍎
        (7..10, bold),  // 试
        (10..13, bold), // ❗
    ];
    assert_eq!(
        crate::editor::view::char_align_highlights(text, &clean),
        clean,
        "已对齐输入应原样保留"
    );
}

// ---------------------------------------------------------------------------
// P1a：渐进打开「纯尾追加」跨块高亮复用
// ---------------------------------------------------------------------------
// P1a / P1c：渐进打开「纯尾追加」跨块高亮复用 + 首屏同步上色（方案 A 改 P1c 旧「首帧
// 延后」行为）
// ---------------------------------------------------------------------------
// 探针（`full_reparse` 计数 / 门控开关）都打在 `Highlighter` **实例**上（`#![cfg(test)]`
// 字段 + 访问器），cargo 并行跑其它触发重解析的测试时不会互相污染。

/// 首屏回归（方案 A，改 P1c 旧行为）：`.rs` 首屏（高亮缓存为空 + 语法已配置）
/// **同步立即上色**——`accept_deferred` 走快路径、不登记 pending、不在「下一帧」才补色
/// （避免「先纯文本、滚动停住才上色」的观感死区）；首帧 `highlight_window` 即整段
/// 建缓存 O(窗口+回看)。滚动大缺口（>单帧预算）仍走既有延后 + settle（见
/// `deferred_scroll_skips_syntect_until_settled`）。
/// `.txt`（无语法）不受影响（空缓存小窗口快路径：建缓存只是填空样式，代价≈0）。
#[test]
fn empty_cache_with_syntax_colors_first_frame_sync() {
    use crate::editor::highlight::Highlighter;
    let text = (0..20)
        .map(|i| format!("fn f{i}() {{ let v = {i}; // c{i} }}\n"))
        .collect::<String>();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    // 空缓存 + 语法 → 首屏同步（本帧不延后、不登记 pending）。
    assert!(
        hl.accept_deferred(0, 10, 16),
        ".rs 首屏（空缓存）应同步立即上色，不在首帧延后"
    );
    assert_eq!(hl.pending(), None, "首屏同步上色不登记 pending");
    // 首帧即全量解析 → 有颜色且与窗口对齐（不必等 settle）。
    let rows = hl.highlight_window(&b, 0, 10, 16).to_vec();
    assert_eq!(rows.len(), 11);
    assert!(!rows[0].is_empty(), "首屏首行应有颜色（无需 settle）");

    // 对照：`.txt`（无语法）首屏空缓存仍走快路径，且首帧填空样式代价≈0。
    let mut hl_txt = Highlighter::new();
    assert!(
        hl_txt.accept_deferred(0, 10, 16),
        "无语法（.txt）首屏走快路径（与方案 A 一致）"
    );
    let txt_rows = hl_txt.highlight_window(&b, 0, 10, 16).to_vec();
    assert_eq!(txt_rows.len(), 11);
    assert!(txt_rows.iter().all(|r| r.is_empty()), ".txt 无语法段（空样式）");
}

// ---------------------------------------------------------------------------
// P1a：渐进打开「纯尾追加」跨块高亮复用
// ---------------------------------------------------------------------------

#[test]
fn highlight_cache_reuses_window_across_append_chunks() {
    // P1a 回归：渐进打开期间追加块纯粹落在当前高亮窗口之下时，不得对可视窗口整段
    // 重解析——旧实现每块 `version+1` 使 `gated_version` 失效 → 每块对窗口 `full_reparse`
    // （大 `.rs` ≈91ms/块 × ~9 块 ≈ 1s 打开卡顿）。缓存（含每行解析状态保存点）只依赖
    // 「窗口内既有行」，尾部追加不改变它们 → 应跨块命中、窗口颜色不变；滚进新块区域时
    // 由 `extend_down` 从保存点续解析补色（正确性用「有颜色」锁住）。
    let chunk1: String = (0..100)
        .map(|i| format!("fn f{i}() {{ let v: u32 = {i}; // c{i} }}\n"))
        .collect();
    let mut b = Buffer::new(&chunk1);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let warm = hl.highlight_window(&b, 0, 40, 16).to_vec();
    assert!(!warm[0].is_empty(), "首窗应有颜色");
    assert_eq!(
        hl.test_reparse_count(),
        1,
        "首窗建缓存恰一次整段重解析"
    );
    let warm_rows = warm.len();

    // 追加 3 块（各 ~50 行，渐进打开路径：Buffer 版本每块 +1）。
    for k in 0..3 {
        let chunk: String = (0..50)
            .map(|i| format!("fn g{k}{i}() {{ let s = \"str{k}{i}\"; /* k{k} */ }}\n"))
            .collect();
        b.append_chunk(&chunk, 30, crate::editor::buffer::LineWidthHistogram::new());
    }
    assert_eq!(b.version(), 3, "3 次追加应推进 3 个版本");

    // 可视窗口仍在顶部（追加区在窗口之下）→ 跨块命中缓存，不再整段重解析。
    let rows = hl.highlight_window(&b, 0, 40, 16).to_vec();
    assert_eq!(rows.len(), warm_rows, "切片仍与请求窗口对齐");
    assert_eq!(
        hl.test_reparse_count(),
        1,
        "追加块落在窗口之下 → 不得触发可视窗口整段重解析"
    );
    assert_eq!(rows, warm, "窗口颜色跨块保持一致（缓存复用）");

    // 滚进新块区域（窗口 [55,105]，缺口 65 行 ≤ 单帧同步预算）→ `extend_down` 从保存点
    // 续解析补色：仍不整段重解析，且新行有正确颜色。
    let deep = hl.highlight_window(&b, 55, 105, 16).to_vec();
    assert_eq!(deep.len(), 51, "深窗切片应与请求窗口对齐");
    assert_eq!(
        hl.test_reparse_count(),
        1,
        "小缺口滚动应走 extend_down，不得整段重解析"
    );
    assert!(
        deep.iter().all(|r| !r.is_empty()),
        "滚进追加块区域后每行都应有颜色（extend_down 补色）"
    );

    // 对照：编辑窗口内的行（非尾追加变更）不得被追加门控吞掉——编辑清掉 `tail_append`
    // 标记 → 门控条件不成立 → 落整段重解析 → 输出与全新解析器对拍一致。
    let mut b2 = b.clone();
    b2.apply_edit(10..14, "X\nY\nZ\n");
    assert_eq!(b2.tail_append(), None, "编辑应清掉『纯尾追加』标记");
    hl.set_dirty(None); // 模拟编辑后的清理：dirty 为 None 时门控也因 tail_append=None 失效
    let edited = hl.highlight_window(&b2, 0, 40, 16).to_vec();
    assert_eq!(hl.test_reparse_count(), 2, "编辑应触发一次整段重解析（门控不吞）");
    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth
        .highlight_window(&Buffer::new(b2.to_string()), 0, 40, 16)
        .to_vec();
    assert_ne!(
        edited, warm,
        "编辑必须在窗口输出中反映（追加门控不得把编辑当纯尾追加命中）"
    );
    assert_eq!(
        edited, truth_rows,
        "编辑后输出应与全新解析器一致"
    );
}

// ---------------------------------------------------------------------------
// P1a before/after 基准（有门控 vs 无门控）
// ---------------------------------------------------------------------------

/// 运行：`cargo test --release --bin gpui-editor bench_progressive_append_highlight_reuse -- \
///        --ignored --nocapture`（debug 亦可，比值不变、绝对值放大）。
#[test]
#[ignore]
fn bench_progressive_append_highlight_reuse() {
    use std::time::Instant;

    let chunk1: String = (0..2_000)
        .map(|i| format!("fn foo{i}() {{ let x: u32 = {i}; // 注释 {i}\n"))
        .collect();
    // 9 块追加（与真实大 .rs 打开块数同量级：首块 256KB + 后 ~1MB×8）。
    let extra: Vec<String> = (0..9)
        .map(|k| {
            (0..400)
                .map(|i| format!("fn g{k}_{i}() {{ let s = \"str{k}{i}\"; /* b {k}{i} */ }}\n"))
                .collect()
        })
        .collect();

    // —— 无门控（= 旧行为）基线 ——
    let (mut b_old, mut hl_old) = (Buffer::new(&chunk1), Highlighter::new());
    hl_old.set_language(Some("rust"));
    hl_old.set_test_append_gate_disabled(true);
    let _ = hl_old.highlight_window(&b_old, 0, 108, 16).to_vec(); // 预热窗口
    let t = Instant::now();
    for chunk in &extra {
        b_old.append_chunk(chunk, 30, crate::editor::buffer::LineWidthHistogram::new());
        let _ = hl_old.highlight_window(&b_old, 0, 108, 16).to_vec();
    }
    let ungated_ms = t.elapsed().as_secs_f64() * 1000.0;
    let ungated_reparse = hl_old.test_reparse_count() - 1; // 减掉预热那一次

    // —— 有门控（本改动） ——
    let (mut b_new, mut hl_new) = (Buffer::new(&chunk1), Highlighter::new());
    hl_new.set_language(Some("rust"));
    hl_new.set_test_append_gate_disabled(false);
    let _ = hl_new.highlight_window(&b_new, 0, 108, 16).to_vec();
    let t = Instant::now();
    for chunk in &extra {
        b_new.append_chunk(chunk, 30, crate::editor::buffer::LineWidthHistogram::new());
        let _ = hl_new.highlight_window(&b_new, 0, 108, 16).to_vec();
    }
    let gated_ms = t.elapsed().as_secs_f64() * 1000.0;
    let gated_reparse = hl_new.test_reparse_count() - 1;

    println!("==== P1a 尾追加高亮复用基准（9 块） ====");
    println!("  无门控(旧): {ungated_ms:.2} ms  整段重解析 ×{ungated_reparse}");
    println!("  有门控(新): {gated_ms:.2} ms  整段重解析 ×{gated_reparse}");
    println!(
        "  提速: {:.1}×",
        ungated_ms / gated_ms.max(f64::MIN_POSITIVE)
    );
}

// ---------------------------------------------------------------------------
// I3：中速大缺口滚动「限预算分段」帧成本基准
// ---------------------------------------------------------------------------

/// I3 基准：中速拖动/惯性滚「每帧缺口 ~500 行」的高亮帧成本（debug = 开发体感）。
/// 模拟 `build_rows` 的帧循环：预热顶窗缓存后连续 8 帧下滚（每帧窗口底越出缓存
/// ~500 行），每帧 `accept_deferred`；同步路径（返回 `quick`）再补 `highlight_window`
/// ——即「UI 线程每帧要付的 syntect 成本」；最后静止一帧 settle。
/// 旧行为（缺口 < 1024 走 `extend_down`）：每帧同步逐行扫整个缺口（~63µs/行，
/// ~500 行 ≈ 40ms+ release / 百 ms 级 debug）→ 滑块跟不上鼠标；
/// 本改动（> `EXTEND_SYNC_BUDGET_ROWS` 一律延后 + settle 有界重锚一屏）：拖动帧≈0
/// syntect（延后帧空样式渲染）、settle 帧≈一屏（O(窗口+回看)）。
/// 运行：`cargo test --bin gpui-editor bench_medium_gap_scroll_frames -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_medium_gap_scroll_frames() {
    use std::time::Instant;
    let text: String = (0..120_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let _ = hl.highlight_window(&b, 0, 108, 16).to_vec(); // 预热（不计时）

    // 拖动：每帧窗口下移 ~500（中速拖动/惯性滚真实缺口带），共 8 帧。
    const GAP: usize = 500;
    const FRAMES: usize = 8;
    let mut lo = 0usize;
    let mut frame_us: Vec<u128> = Vec::with_capacity(FRAMES);
    let mut sync_n = 0usize;
    for _ in 0..FRAMES {
        lo += GAP;
        let hi = lo + 108;
        let t = Instant::now();
        if hl.accept_deferred(lo, hi, 16) {
            sync_n += 1; // 同步路径（旧行为）：本帧整段扩展缺口。
            let _ = hl.highlight_window(&b, lo, hi, 16).to_vec();
        } // 延后路径（本改动）：本帧 0 syntect，颜色缺席由 settle 补。
        frame_us.push(t.elapsed().as_micros());
    }
    let mean_us = frame_us.iter().sum::<u128>() as f64 / FRAMES as f64;
    let min_us = *frame_us.iter().min().unwrap();
    // 静止：pending 存在则一次有界重锚（O(窗口+回看)≈一屏）上色。
    let settle_us = if hl.pending().is_some() {
        let t = Instant::now();
        assert!(hl.settle_pending(&b), "settle 应补齐缓存");
        t.elapsed().as_micros()
    } else {
        0
    };
    println!("==== I3 中速大缺口滚动帧基准（缺口 {GAP} 行 ×{FRAMES} 帧） ====");
    println!(
        "  拖动帧: 均值 {mean_us:.0}µs  最小 {min_us}µs  明细 {frame_us:?}（同步补缺口 {sync_n} 帧）"
    );
    println!("  静止 settle（一屏有界重锚）: {settle_us}µs");
}

/// 轨 C 基准（ignored）：滚动帧时间直方图——中速大缺口连续滚动 N 帧，每帧 `Instant`
/// 记录刷新墙钟 → 打 min/median/p95 + 掉帧计数（单帧 >16.7ms 记掉帧）。
/// 复用 [`bench_medium_gap_scroll_frames`] 同款帧循环与大文档（120k 行 / 缺口 500 行），
/// 纯高亮帧路径（syntect 段）＝编辑区滚动帧成本的构成主体。
/// 运行：`cargo test --bin gpui-editor bench_scroll_frame_histogram -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_scroll_frame_histogram() {
    use std::time::Instant;
    let text: String = (0..120_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let mut hl = Highlighter::new();
    hl.set_language(Some("rust"));
    let _ = hl.highlight_window(&b, 0, 108, 16).to_vec(); // 预热（不计时）
    const GAP: usize = 500;
    const FRAMES: usize = 90;
    let mut lo = 0usize;
    let mut frame_ms: Vec<f64> = Vec::with_capacity(FRAMES);
    for _ in 0..FRAMES {
        lo += GAP;
        let hi = lo + 108;
        let t = Instant::now();
        if hl.accept_deferred(lo, hi, 16) {
            let _ = hl.highlight_window(&b, lo, hi, 16).to_vec();
        }
        frame_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let mut s = frame_ms.clone();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = s[0];
    let median = s[s.len() / 2];
    let p95 = s[(s.len() as f64 * 0.95) as usize - 1];
    let dropped = frame_ms.iter().filter(|&&f| f > 16.7).count();
    println!("==== 滚动帧时间直方图（缺口 {GAP} 行 ×{FRAMES} 帧） ====");
    println!(
        "  min={min:.3}ms median={median:.3}ms p95={p95:.3}ms 掉帧(>16.7ms)={dropped}/{FRAMES}"
    );
    println!("  明细(前 20): {:?}", &frame_ms[..20.min(FRAMES)]);
}

// ---------------------------------------------------------------------------
// 首屏上色时延基准（方案 A+B）
// ---------------------------------------------------------------------------

/// 首屏「首帧 → 可见区着色可感知」基准：模拟 `build_rows` 的首屏帧循环（`accept_deferred`
/// 快/延后分叉 + settle），输出到「可见区全有色」所需**帧数与 syntect 成本**（debug）。
/// - 方案 A：首帧 `accept_deferred` 走快速（空缓存 → 同步）→ 首帧 `highlight_window`
///   即着色（**1 帧到位**）；首屏预热（≤2×视口，封顶 160）使缓存行数有界（打印
///   `test_cache_len`，
///   绝不当全档 parse）。
/// - 旧行为（P1c）：首帧延后（本帧 0 syntect、空样式 + RAF），次帧 settle 才着色
///   （**2 帧到位**）——即「先纯文本、随后才补色」的观感死区。
/// 基准实测当前代码实际路径；before/after 各跑 ≥3 次取中位数对比（`first_quick` + 帧数
/// + µs 中位数），bench 代码两份 HEAD 相同。
/// 运行：`cargo test --bin gpui-editor bench_first_frame_coloring -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_first_frame_coloring() {
    use std::time::Instant;
    const W: usize = 108;
    const RUNS: usize = 5;
    let text: String = (0..120_000)
        .map(|i| match i % 4 {
            0 => format!("fn foo{i}() {{ // c{i}\n"),
            1 => format!("let x: u32 = {i} + 1;\n"),
            2 => format!("core::writeln!(\"fmt {i}\");\n"),
            _ => format!("struct S{i} {{ a: u32, }}\n"),
        })
        .collect();
    let b = Buffer::new(&text);
    let fresh = || {
        let mut hl = Highlighter::new();
        hl.set_language(Some("rust"));
        hl
    };

    let mut first_quick = None;
    let mut frames_first = None;
    let mut colors_us: Vec<u128> = Vec::new(); // 当前代码实际路径（到首色 syntect 成本）。
    let mut cache_len = 0usize;
    for _ in 0..RUNS {
        let mut hl = fresh();
        let quick = hl.accept_deferred(0, W, 16);
        if first_quick.is_none() {
            first_quick = Some(quick);
        }
        let t = Instant::now();
        if quick {
            // 方案 A：首帧同步上色（含 ≤2×视口预热）。
            if frames_first.is_none() {
                frames_first = Some(1usize);
            }
            let rows = hl.highlight_window(&b, 0, W, 16).to_vec();
            assert!(rows.iter().all(|r| !r.is_empty()), "首屏即可见区全有色");
        } else {
            // 旧行为：首帧延后（本帧 0 syntect、空样式 + RAF）；次帧窗口一致 → settle 补色。
            if frames_first.is_none() {
                frames_first = Some(2usize);
            }
            let prev = hl.pending();
            let quick2 = hl.accept_deferred(0, W, 16);
            assert!(!quick2 && prev == Some((0, W, 16)), "次帧仍延后 → 满足 settle 前提");
            assert!(hl.settle_pending(&b), "settle 应补齐缓存");
            let rows = hl.highlight_window(&b, 0, W, 16).to_vec();
            assert!(rows.iter().all(|r| !r.is_empty()), "settle 后可见区应全有色");
        }
        colors_us.push(t.elapsed().as_micros());
        cache_len = cache_len.max(hl.test_cache_len());
    }
    let mut us = colors_us.clone();
    us.sort_unstable();
    let med = us[us.len() / 2];
    // 校准：同行文本绕过 Buffer 直接纯 syntect parse，看每行成本大头在哪。
    let st_ref = crate::editor::highlight::syntax_set_ref();
    let syn = st_ref.find_syntax_by_token("rust").unwrap();
    let mut pp = syntect::parsing::ParseState::new(syn);
    let t_c = std::time::Instant::now();
    for r in 0..269 {
        let _ = pp.parse_line(b.line_text(r).as_ref(), st_ref);
    }
    let cal_us = t_c.elapsed().as_micros();
    println!("==== 首屏上色时延基准（大 .rs 12 万行，debug，视口 {W} 行 ×{RUNS} 次）====");
    println!("  校准：纯 syntect parse 269 行 = {cal_us}µs（{:.0}µs/行）", cal_us as f64 / 269.0);
    println!("  首帧 accept_deferred 返回快速（方案 A 同步上色）: {first_quick:?}");
    println!("  到「可见区全有色」: {frames_first:?} 帧（1=首帧即有色；2=先纯文本+RAF，次帧 settle 才着色）");
    println!("  着色 syntect 成本: 中位 {med}µs  min {}µs  {us:?}", us[0]);
    println!("  首屏预热缓存行数（封顶=非全档 parse）: {cache_len}（基线窗 {W} 行）");
}
