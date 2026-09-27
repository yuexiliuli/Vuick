//! 编辑器组件测试： 纯逻辑（buffer/selection/movement/undo/highlight）+ 视图
//! （渲染、输入、撤销、选区、鼠标）。

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
    let prefix = format!("aaXYccc\n");
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
    let prefix = format!("xshort\n这是最长的一行here\nend\n");
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
    use gpui_component::input::Rope;

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
    let prefix = format!("short\n这是最长行here\nend\n");
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

#[test]
fn far_down_jump_reanchors_bounded_not_full_prefix() {
    // 回归：大文档 `.rs` 拖滚动条时滑块极短，1 鼠标像素 ≈ 数千行，缺口（目标窗口底 −
    // 缓存上界）巨大。修复前 `extend_down` 会把整个缺口逐行重跑 syntect（16 万行级 →
    // 数百毫秒），UI 线程卡死 → 内容只有松开才动。修复后缺口 >`REANCHOR_GAP` 改走
    // **有界重锚**（O(窗口+回看) ≈ 百行级）。测试用 wall-clock 兜底 + 切片/锚点断言：
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

    // 首帧：顶部窗口（快路径，覆盖/小扩展）。
    assert!(hl.accept_deferred(0, 108, 16), "顶部窗口应走快速路径");
    assert_eq!(hl.pending(), None, "快路径不登记 pending");
    let top = hl.highlight_window(&b, 0, 108, 16).to_vec();
    assert!(!top[0].is_empty(), "快速路径应有颜色");

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
    // 暖缓存：小窗口 [0,20]（缓存只覆盖 0..≈36）
    let _ = hl.highlight_window(&b, 0, 20, 16).to_vec();
    // 编辑第 0 行行首插入换行（净行 +1）
    let pos = b.line_start(0);
    b.apply_edit(pos..pos, "\n");
    hl.set_dirty(Some((0, 2, 1)));
    // 请求窗口大幅下移 [0,60]：底超出旧缓存上界 + delta → 尾部位移行越出旧缓存。
    let after: Vec<_> = hl.highlight_window(&b, 0, 60, 16).to_vec();
    let mut truth = Highlighter::new();
    truth.set_language(Some("rust"));
    let truth_rows: Vec<_> = truth.highlight_window(&b, 0, 60, 16).to_vec();
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
    let runs = rows.get(0).cloned().unwrap_or_default();
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
            background_color: Some(gpui::rgb(0x0078d4).into()),
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
    let runs = rows.get(0).cloned().unwrap_or_default();
    assert!(!runs.is_empty(), "rust 行应有语法段");
    // `merge_highlights` 现在消费「仅色」的 `HighlightStyle` 段（syntect `(Range, Hsla)`
    // 由 `build_rows` 转换；这里等价转换喂入）。
    let runs: Vec<(std::ops::Range<usize>, HighlightStyle)> = runs
        .iter()
        .map(|(r, c)| (r.clone(), HighlightStyle { color: Some(*c), ..Default::default() }))
        .collect();

    let sel_bg = gpui::rgb(0x0078d4).opacity(0.28).into();
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
fn markdown_editor_renders_wysiwyg_styles(cx: &mut TestAppContext) {
    use std::ops::Range;
    use gpui::HighlightStyle;

    // Phase 1：`.md` 编辑区行渲染走「所见即所得」滤镜——记号淡化、正文富样式，
    // 且底层纯文本完全不变（光标/选区/撤销/保存走原路径，无「视觉↔字节」映射）。
    let text = "# 标题\n\n**加粗** `code` [链接](https://x) 与 *斜体*\n- 列表项\n> 引用\n";
    let (host, mut cx) = open(cx, text);

    // 非 md 语言不受滤镜影响（回归护栏：syntect 路径保持可用）
    let _rust_row = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("rust")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });

    // 切到 markdown → 启用滤镜
    let row0 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.set_language(Some("markdown")));
        ed.update(cx, |ed, _| ed.test_line_styles(window, 0))
    });
    // 标题行：`#` 记号淡化 + 正文加粗
    assert!(
        row0.iter().any(|(_, s)| s.fade_out.is_some()),
        "标题记号（#）应淡化: {row0:?}"
    );
    assert!(
        row0.iter().any(|(_, s)| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "标题正文应加粗: {row0:?}"
    );

    // 第 3 行：**加粗** `code` [链接](https://x) 与 *斜体*
    let row2 = cx.update(|window, cx| {
        let ed = host.read(cx).ed.clone();
        ed.update(cx, |ed, _| ed.test_line_styles(window, 2))
    });
    let has = |row: &[(Range<usize>, HighlightStyle)], pred: fn(&HighlightStyle) -> bool| {
        row.iter().any(|(_, s)| pred(s))
    };
    assert!(
        has(&row2, |s| s.font_weight == Some(gpui::FontWeight::BOLD)),
        "应渲染加粗: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.font_style == Some(gpui::FontStyle::Italic)),
        "应渲染斜体: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.background_color.is_some()),
        "行内代码应有底色: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.fade_out.is_some()),
        "记号应淡化: {row2:?}"
    );
    assert!(
        has(&row2, |s| s.underline.is_some()),
        "链接应有下划线: {row2:?}"
    );

    // 底层文本保持不变（所见≠所得底稿仍是纯 Markdown）
    assert_eq!(editor_text(&host, &mut cx), text);
}

#[gpui::test]
fn font_size_change_keeps_viewport_top_anchored(cx: &mut TestAppContext) {
    // 回归（2026-09-19 用户反馈）：编辑区 Ctrl+滚轮放大/缩小字号时窗口也跟着滚动。
    // 修复 = 行高变化时把滚动目标同比换算（同 PDF 的视口锚定）——视口顶部的内容点
    // 不随行高变化跳走。这里直接改字号覆盖（等价于 Ctrl+滚轮 / Ctrl+=/- 的落地路径），
    // 断言「滚动 px 按行高比缩放、顶部绝对视觉行不变」。
    let text = (1..=300).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
    let (host, mut cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    // 普通滚轮（非 Ctrl）滚到中段 → 作为缩放前的视口锚点。
    let b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(b.origin.x.as_f32() + b.size.width.as_f32() / 2.0),
        px(b.origin.y.as_f32() + b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-3000.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(&mut cx);
    let (d1, lh1, f0) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1, ed.font_size_px())
    });
    assert!(d1 > 0., "应先滚到中段（d1={d1}）");
    // 放大字号 → 行高变大 → 滚动目标应按行高比缩放（视口顶部内容点锚定）。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 + 6.0));
        cx.notify();
    });
    settle(&mut cx);
    let (d2, lh2) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh2 > lh1, "放大后行高应变大：{lh1} → {lh2}");
    assert!(
        (d2 - d1 * (lh2 / lh1)).abs() < lh1 * 0.5,
        "滚动目标应按行高比缩放（锚定视口）：{d1} → {d2}（期望 {:.1}）",
        d1 * (lh2 / lh1)
    );
    let row1 = (d1 / lh1).floor();
    let row2 = (d2 / lh2).floor();
    assert!(
        (row2 - row1).abs() <= 2.0,
        "缩放后顶部所在绝对视觉行应不变：{row1} → {row2}"
    );
    // 缩小（回到略小于初始）→ 反向同样锚定。
    ed.update(cx, |ed, cx| {
        ed.set_font_override(Some(f0 - 2.0));
        cx.notify();
    });
    settle(&mut cx);
    let (d3, lh3) = cx.update(|_, cx| {
        let ed = ed.read(cx);
        (ed.test_desired_scroll_px(), ed.test_metrics().1)
    });
    assert!(lh3 < lh2, "缩小后行高应变小：{lh2} → {lh3}");
    assert!(
        (d3 - d1 * (lh3 / lh1)).abs() < lh1 * 0.5,
        "缩小同样按行高比锚定：{d1} → {d3}（期望 {:.1}）",
        d1 * (lh3 / lh1)
    );
}

#[gpui::test]
fn editor_bottom_pad_quantized_to_line_height(cx: &mut TestAppContext) {
    // 回归（专家评审 P4）：底部预留（横向滚动条厚 16px + 额外行高）向上量化到
    // line_height 整数倍——否则滚到底视口停在「行间半格」上，最后一行加载/点击
    // 命中/光标出现次像素误差。断言 pad 是行高的整数倍且 ≥ 一行。
    let (host, cx) = open(cx, "row0\nrow1\nrow2");
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    let frac = pad / lh;
    assert!(
        (frac - frac.round()).abs() < 1e-3,
        "底部预留 {pad:.2}px 应为行高 {lh:.2}px 的整数倍，实际 {} 行",
        frac
    );
    assert!(pad >= lh, "底部预留应至少覆盖一行（{pad:.2} ≥ {lh:.2}）");
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
fn fold_block_heuristic_detects_indent_blocks(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}\nfn bar() {\n    c();\n}";
    let (host, cx) = open(cx, text);
    let ends = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        // 0 行块：缩进 0，直到同缩进 "}" → [0,3)；4 行块（单行体）→ [4,6)。
        (ed.test_fold_block_end(0), ed.test_fold_block_end(4))
    });
    assert_eq!(ends, (Some(3), Some(6)));
    // 文件末行无后续块 → 不可折叠。
    let none = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_block_end(6));
    assert_eq!(none, None);
}

#[gpui::test]
fn fold_reduces_scroll_range_and_expand_restores(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, mut cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(&mut cx);
    let folded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    // 折叠 [5,10) 隐藏 4 行 → 量程条目数应相对全文减少。
    assert!(folded < 20 + 8, "折叠后量程条目数应缩小（folded={folded}）");
    // 再次 toggle（同为 start）→ 展开恢复。
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(&mut cx);
    let expanded = cx.update(|_, cx| host.read(cx).ed.read(cx).test_scroll_item_count());
    assert!(expanded > folded, "展开后量程条目数应恢复（{folded} → {expanded}）");
}

#[gpui::test]
fn click_maps_through_fold_to_logical_line(cx: &mut TestAppContext) {
    // 20 行折叠 [5,10)（隐藏 6..9）。视觉行 7 = 逻辑行 11：点击应命中 11 行行首。
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, mut cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(&mut cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    let y = px(lh * 7.5); // 视觉行 7
    let x = px(4.); // 行号列内 → 列 0
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(&mut cx);
    let head = editor_selection(&host, &mut cx).head();
    let expected = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(11, 0));
    assert_eq!(head, expected, "点击视觉行 7 应命中折叠后的逻辑行 11（{head} vs {expected}）");
}

#[gpui::test]
fn gutter_click_toggles_active_fold(cx: &mut TestAppContext) {
    let text = (0..20).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, mut cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(5, 10)));
    settle(&mut cx);
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 点击折叠首行（视觉行 5）gutter → 展开。
    let y = px(lh * 5.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(&mut cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "点击折叠首行 gutter 应展开折叠"
    );
}

#[gpui::test]
fn typing_inside_folded_body_expands_fold(cx: &mut TestAppContext) {
    let (host, mut cx) = open(cx, "aaa\nbbb\nccc\nddd");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.test_set_fold(1, 3))); // 隐藏行 2
    settle(&mut cx);
    // 把光标直接放进隐藏行 2 中间，键入 → 自动展开并写入。
    let off = cx.update(|_, cx| host.read(cx).ed.read(cx).offset_for_line_col(2, 1));
    cx.update(|_, cx| ed.update(cx, |ed, _| ed.set_caret_to(off)));
    focus(&host, cx);
    cx.simulate_keystrokes("X");
    settle(&mut cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "编辑折叠体内应自动展开折叠"
    );
    // 行 0="aaa" 行 1="bbb" 行 2="ccc"（被折叠隐藏）行 3="ddd"：在行 2 第 1 列插 X。
    assert_eq!(editor_text(&host, &mut cx), "aaa\nbbb\ncXcc\nddd");
}

#[gpui::test]
fn ctrl_shift_bracket_folds_and_unfolds(cx: &mut TestAppContext) {
    let text = "fn foo() {\n    a();\n    b();\n}";
    let (host, mut cx) = open(cx, text);
    focus(&host, cx);
    let w = window_handle(&mut cx);
    // 光标在 0 行：Ctrl+Shift+[ → 折叠 [0,3)。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-[").unwrap());
    settle(&mut cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 0, end: 3 }]);
    // Ctrl+Shift+] → 展开。
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-shift-]").unwrap());
    settle(&mut cx);
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "Ctrl+Shift+] 应展开折叠"
    );
}

#[gpui::test]
fn ruler_x_computed_from_metric_and_moves_with_hscroll(cx: &mut TestAppContext) {
    // 长行文档让横向可视范围/滚动条可用。
    let text = "x".repeat(300);
    let (host, mut cx) = open(cx, &text);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    // 测试窗 1920 宽 > 80 列标尺位置 → 标尺在可视区。
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x0 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺应在可视区");
    assert!(
        (x0 - (gutter + 80.0 * cw)).abs() < 1.0,
        "标尺 x 应 = gutter + col*char_width（x0={x0}）"
    );
    // 横向滚动 ≥5 格 → 标尺随内容左移约 300px。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 50.),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(300.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(&mut cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "应真正横向滚动（hscroll={hscroll}）");
    let x1 = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("80 列标尺滚动后仍应在可视区");
    assert!(
        (x1 - (x0 - hscroll)).abs() < 1.0,
        "标尺应随内容左移 hscroll（x0={x0} x1={x1} hscroll={hscroll}）"
    );
}

#[gpui::test]
fn fold_candidates_detected_for_indented_blocks(cx: &mut TestAppContext) {
    // 打开即自动探测：内层 if 块 + 外层函数块（可嵌套）。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, cx) = open(cx, text);
    let cands = cx.update(|_, cx| host.read(cx).ed.read(cx).test_fold_candidates().to_vec());
    assert_eq!(cands, vec![Fold { start: 1, end: 3 }, Fold { start: 0, end: 5 }]);
    // gutter 指示器口径：候选起点行有标记。
    let marks = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        let ed = ed.read(cx);
        (
            ed.test_has_fold_or_candidate(0),
            ed.test_has_fold_or_candidate(1),
            ed.test_has_fold_or_candidate(2),
        )
    });
    assert_eq!(marks, (true, true, false));
}

#[gpui::test]
fn gutter_click_on_candidate_creates_fold(cx: &mut TestAppContext) {
    // 候选已自动探测但未激活：点击 candidate 起点行的 gutter → 创建激活折叠。
    let text = "fn a() {\n    if x {\n        y();\n    }\n    z();\n}";
    let (host, mut cx) = open(cx, text);
    // 确认尚未激活。
    assert!(
        cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().is_empty()),
        "打开时不应有激活折叠"
    );
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1).max(1.);
    // 视觉行 1 = 逻辑行 1（if 块候选）。
    let y = px(lh * 1.5);
    let x = px(2.);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(&mut cx);
    let folds = cx.update(|_, cx| host.read(cx).ed.read(cx).test_folds().folds().to_vec());
    assert_eq!(folds, vec![Fold { start: 1, end: 3 }], "点击候选起点应创建激活折叠");
}

#[gpui::test]
fn ruler_infra_renders_but_defaults_off(cx: &mut TestAppContext) {
    // margin guide 默认关闭（EDITOR_RULER_COLUMNS 为空）：叠层容器仍渲染、无竖线子元素；
    // 坐标换算（第 80 列在可视区内应给 Some(x)）保持精确，供用户配置恢复使用。
    let text = "x".repeat(300);
    let (host, cx) = open(cx, &text);
    let (cw, _, gutter) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let avail_w = cx.update(|_, cx| host.read(cx).ed.read(cx).test_content_w_px());
    let x = cx
        .update(|_, cx| host.read(cx).ed.read(cx).test_ruler_x(80, avail_w))
        .expect("第 80 列在可视区内应可通过单纯换算得到 x");
    assert!((x - (gutter + 80.0 * cw)).abs() < 1.0, "标尺换算 = gutter + col*char_width");
    assert!(
        cx.debug_bounds("ce-rulers").is_some(),
        "标尺叠层容器仍应在渲染树中"
    );
    // 默认列数为空 → 不渲染任何 ce-ruler-line（margin guide 默认隐藏）。
    assert!(
        cx.debug_bounds("ce-ruler-line").is_none(),
        "默认不渲染第 80 列竖线（用户要的是括号标尺）"
    );
}

#[gpui::test]
fn bracket_guides_detected_and_rendered(cx: &mut TestAppContext) {
    // 用户示例：三对跨行花括号 → 三条括号标尺竖线。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let pairs = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bracket_pairs().to_vec());
    assert_eq!(
        pairs,
        vec![
            crate::editor::fold::BracketPair { open_line: 2, open_col: 13, close_line: 4, guide_units: 8 },
            crate::editor::fold::BracketPair { open_line: 1, open_col: 23, close_line: 5, guide_units: 4 },
            crate::editor::fold::BracketPair { open_line: 0, open_col: 13, close_line: 6, guide_units: 0 },
        ]
    );
    // 竖线叠层确实渲染进树。
    assert!(
        cx.debug_bounds("ce-bracket-guides").is_some(),
        "括号标尺叠层应渲染"
    );
}

#[gpui::test]
fn bracket_guide_segments_span_each_pair(cx: &mut TestAppContext) {
    use crate::config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 与用户样例同构。注意 spans：竖线从开行**之后**画到闭行**内约半行**（贴 `}` 字形），
    // 端点微调一律按 config 常量算期望（用户调 HEAD/TAIL 后不打红）。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}";
    let (host, cx) = open(cx, text);
    let (cw, lh, _g) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let harea_w = 1200.0;
    let view_h = 1000.0;
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(0, 0.0, lh, harea_w, view_h)
    });
    // 三条竖线：x = 块首行缩进列 0/4/8；跨度 [开行+1, 闭行) 底端再伸 TAIL、顶部缩 HEAD。
    assert_eq!(segs.len(), 3, "三对跨行花括号 → 三条竖线");
    // 外层 impl 块：x≈0cw，y=(1+HEAD)lh 起、高 (5−HEAD+TAIL)lh（行 1..5 + 尾巴）。
    let impl_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (1.0 + HEAD) * lh).abs() < 0.5
                && (*h - (5.0 - HEAD + TAIL) * lh).abs() < 0.5
        })
        .expect("应有 impl 块竖线");
    assert!(
        (impl_guide.0 - 0.0 * cw).abs() < 1.0,
        "外层竖线应在块首列 0×字宽，实际 {}",
        impl_guide.0
    );
    // Self 块：x≈8cw，y=(3+HEAD)lh 起、高 (1−HEAD+TAIL)lh（行 3 + 尾巴）。
    let self_guide = segs
        .iter()
        .find(|(_, y, h)| {
            (*y - (3.0 + HEAD) * lh).abs() < 0.5
                && (*h - (1.0 - HEAD + TAIL) * lh).abs() < 0.5
        })
        .expect("应有 Self 块竖线");
    assert!(
        (self_guide.0 - 8.0 * cw).abs() < 1.0,
        "内层竖线应在块首列 8×字宽，实际 {}",
        self_guide.0
    );
}

#[gpui::test]
fn bracket_guide_overlapping_same_column_merged(cx: &mut TestAppContext) {
    use crate::config::layout::{
        BRACKET_GUIDE_CLOSE_TAIL_LINES as TAIL, BRACKET_GUIDE_OPEN_HEAD_LINES as HEAD,
    };
    // 同列（缩进 0）嵌套的两个块：原会画两条重叠竖线（视觉加粗/变色），合并成一根。
    let text = "fn a() {\n{\n    x();\n}\n}";
    let (host, cx) = open(cx, text);
    let (_, lh, _) = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics());
    let lh = lh.max(1.);
    let segs = cx.update(|_, cx| {
        let ed = host.read(cx).ed.clone();
        ed.read(cx).test_bracket_guide_segments(0, 0.0, lh, 1200.0, 1000.0)
    });
    // 两根（fn a 的 [1,4) 与内层 { 的 [2,3)）同列 → 合并成一根 [1,4)，端点半行微调。
    assert_eq!(segs.len(), 1, "同列重叠竖线应合并为一根");
    assert!(
        (segs[0].1 - (1.0 + HEAD) * lh).abs() < 0.5
            && (segs[0].2 - (3.0 - HEAD + TAIL) * lh).abs() < 0.5,
        "合并段应覆盖行 1..3 + 端点半行（实际 y={} h={}）",
        segs[0].1,
        segs[0].2
    );
}

#[gpui::test]
fn guide_click_highlights_pair_and_elsewhere_clears(cx: &mut TestAppContext) {
    // 语义（用户确认）：标尺不可点选；点体内某行 → 点亮该行所属**最内层**括号块竖线；
    // 点非块内行（如顶层）→ 清除。命中断言：行 3 ∈ Self 块 [2,4)、行 2 = 开行 → 外层。
    let text = "impl RowData {\n    fn empty() -> Self {\n        Self {\n            a: 1,\n        }\n    }\n}\nfn other() {}";
    let (host, cx) = open(cx, text);
    // 口径断言（不依赖鼠标坐标）。
    let (case, case2, case7) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        (
            ed.test_guide_for_logical_row(3),
            ed.test_guide_for_logical_row(2),
            ed.test_guide_for_logical_row(7),
        )
    });
    assert_eq!(case, Some(2), "行 3 ∈ Self 块 [2,4) → 点亮 Self 开行 2");
    assert_eq!(case2, Some(1), "行 2 = Self 开行（非块内）→ 归外层 fn 块");
    assert_eq!(case7, None, "行 7 = 顶层 fn 行 → 无命中");
    let (lh, gutter) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx).test_metrics();
        (ed.1.max(1.), ed.2)
    });
    let mods = Modifiers::default();
    // 点体内（行 3 中部，x 取 gutter+10px → 文本区）。
    let y = px(lh * 3.5);
    let x = px(gutter + 10.);
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    // 点亮 Self 块。
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide());
    assert_eq!(lit, Some(2), "点行 3 应点亮 Self 块（open_line=2）");
    // 点顶层行 7 → 清除。
    let y7 = px(lh * 7.5);
    cx.simulate_mouse_move(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y7), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y7), MouseButton::Left, mods);
    let lit = cx.update(|_, cx| host.read(cx).ed.read(cx).test_active_guide());
    assert_eq!(lit, None, "点顶层行 7 应清除点亮");
}

#[gpui::test]
fn mouse_click_lands_on_ascii_char_boundaries(cx: &mut TestAppContext) {
    // 回归护栏：等宽 ASCII 命中测试在接入真实行布局后仍应精确落在字符边界。
    let line = "abcdef";
    let (host, mut cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 1usize), (2, 2), (3, 3), (6, 6)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(&mut cx);
        assert_eq!(
            editor_selection(&host, &mut cx).head(),
            expected_head,
            "点击第 {k} 个 ASCII 字符右边界应落在第 {k} 字符后"
        );
    }
}

#[gpui::test]
fn mouse_click_lands_on_cjk_char_boundaries(cx: &mut TestAppContext) {
    // 回归（2026-09-15 真机反馈）：中文行点击中间输入，实际偏移到更后面。
    // 根因：命中测试按「每字符 = 1×ASCII 字符宽」等宽近似，而 CJK 全角字实际
    // 宽 2× → 点击第 k 个字被算成第 2k 个字。现在用真实行布局
    // `closest_index_for_x` 做像素→字符边界映射，点击即落点。
    let line = "中华文字"; // 4 个全角字：每字 3 字节
    let (host, mut cx) = open(cx, line);
    focus(&host, cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let y = px(lh * 0.5);
    for (k, expected_head) in [(1usize, 3usize), (2, 6), (3, 9), (4, 12)] {
        let x = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, expected_head))
        });
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
        settle(&mut cx);
        let head = editor_selection(&host, &mut cx).head();
        assert_eq!(head, expected_head, "点击第 {k} 个全角字右边界应落在第 {k} 字后");
    }
}

#[gpui::test]
fn mouse_drag_selects_across_rows_rust_vs_plain(cx: &mut TestAppContext) {
    // 复现调查：大文档测试.rs 无法鼠标选择、而 .txt/.md 正常。
    // 组件内鼠标逻辑与语言无关，此测试把「带 Rust 高亮」与「纯文本」并排验证，
    // 确认是否为语言/高亮路径引起。
    for lang in [Some("rust"), Some("markdown"), None] {
        let text = "fn main() {\n    let s = \"hello\";\n    // 注释\n}\n";
        let (host, mut cx) = open(&mut *cx, text);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        ed.update(cx, |ed, _| ed.set_language(lang));
        focus(&host, &mut cx);
        settle(&mut cx);
        let ed = cx.update(|_, cx| host.read(cx).ed.clone());
        let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
        let x1 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 0, 2))
        });
        let x2 = cx.update(|window, cx| {
            px(gutter + ed.read(cx).test_char_boundary_x(window, 2, 3))
        });
        let y1 = px(lh * 0.5);
        let y2 = px(lh * 2.5);
        let mods = Modifiers::default();
        cx.simulate_mouse_move(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_down(point(x1, y1), MouseButton::Left, mods);
        cx.simulate_mouse_move(point(x2, y2), MouseButton::Left, mods);
        cx.simulate_mouse_up(point(x2, y2), MouseButton::Left, mods);
        settle(&mut cx);
        let sel = editor_selection(&host, &mut cx);
        assert!(
            !sel.is_collapsed(),
            "lang={lang:?}: 拖选应形成选区，sel={sel:?}"
        );
        assert_eq!(sel.range().start, 2, "lang={lang:?} 选区起点");
        assert_eq!(
            sel.range().end,
            "fn main() {\n    let s = \"hello\";\n".len() + 3,
            "lang={lang:?} 选区终点应在第 3 行第 4 字符后"
        );
    }
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
fn gutter_fixed_while_content_scrolls_horizontally(cx: &mut TestAppContext) {
    // 回归（2026-09-15 用户反馈）：编辑区内容横向滚动时，行号也跟着滚走。
    // 现在行号是编辑区左缘的固定列（`ce-gutter`，独立 List 与文本锁步）——
    // 横向滚动内容后行号列左缘必须纹丝不动，而文本内容必须真的被推走。
    // 测试窗 1920px 宽 → 最长行需远超可视区才有横向可滚量程。
    let long = "一".repeat(600);
    let (host, mut cx) = open(cx, &format!("{long}\n第二行"));
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let gutter0 = cx
        .debug_bounds("ce-gutter")
        .expect("行号固定列应渲染");
    assert!(
        gutter0.size.width.as_f32() > 0.,
        "行号列应有宽度：{gutter0:?}"
    );
    // 向编辑区中心派发横向滚轮（向右 = 内容左移，正 delta）。事件坐标与
    // `debug_bounds` 同空间（不能用 `window.bounds()`——那是平台坐标）。
    let ed_b = cx.debug_bounds("code-editor").expect("编辑区应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + ed_b.size.width.as_f32() / 2.0),
        px(ed_b.origin.y.as_f32() + ed_b.size.height.as_f32() / 2.0),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(120.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(&mut cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "横向滚动应真正发生（hscroll={hscroll}）");
    let gutter1 = cx.debug_bounds("ce-gutter").expect("行号列仍在");
    assert_eq!(
        gutter0.origin.x,
        gutter1.origin.x,
        "行号列左缘不应随内容横向滚动（{gutter0:?} → {gutter1:?}）"
    );
    assert!(
        gutter1.origin.x.as_f32().abs() < 1.0,
        "行号应贴在编辑区左缘（origin.x={}）",
        gutter1.origin.x.as_f32()
    );
}

#[gpui::test]
fn ime_composition_replaces_marked_text_and_commits(cx: &mut TestAppContext) {
    // 回归：IME 组合曾把每次 `replace_and_mark(None, …)` 追加到旧组合后面
    // （产生 'nn'...垃圾残留），提交后也不清 marked → 下次组合把已提交文本当
    // 旧 marked 再次替换。参照 gpui-component 的组合流：None 区间 = 替换当前
    // marked；普通 `replace_text_in_range` 提交后自动结束组合。
    let (host, mut cx) = open(cx, "");
    focus(&host, cx);
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "prefix ", window, cx);
            // 组合第 1 字：n → ni → 你（每次更新替换旧的 marked）
            ed.replace_and_mark_text_in_range(None, "n", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "你", None, window, cx);
            // 提交第 1 字（结束本次组合，marked 清空）
            ed.replace_text_in_range(None, "你", window, cx);
            // 组合第 2 字并提交
            ed.replace_and_mark_text_in_range(None, "h", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "ha", None, window, cx);
            ed.replace_and_mark_text_in_range(None, "好", None, window, cx);
            ed.replace_text_in_range(None, "好", window, cx);
        });
    });
    assert_eq!(editor_text(&host, &mut cx), "prefix 你好");
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), "prefix 你好".len());
    // 提交后再开一次组合，不应把已提交的 "prefix 你好" 当旧 marked 覆盖
    cx.update(|window, app| {
        let ed = host.read(app).ed.clone();
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "x", None, window, cx);
            ed.replace_text_in_range(None, "!", window, cx);
        });
    });
    assert_eq!(editor_text(&host, &mut cx), "prefix 你好!");
}

#[gpui::test]
fn ime_new_selected_range_is_relative_to_marked_text(cx: &mut TestAppContext) {
    // 回归 1（崩溃）：`new_selected_range` 曾被当「文档绝对 UTF-16 偏移」→ 光标
    // 跳到文档开头附近，落点多字节字符中间时后续插入触发
    // `byte index is not on a char boundary` panic（buffer.rs apply_edit）。
    // 回归 2（跳开头）：同一根因让大文档任意位置输入中文时插入位置跑回文首。
    let (host, mut cx) = open(cx, "一二三四五");
    let ed = cx.update(|_, app| host.read(app).ed.clone());
    // 光标移到文末（第 5 字后，字节 15）
    ed.update(cx, |ed, _| ed.set_caret_to(15));
    // 文末开始组合 "ni"，IMS 光标停在组合内第 1 个 UTF-16 单元后（相对 1..1）
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "ni", Some(1..1), window, cx);
        });
    });
    // 光标 = 文末 + 组合内偏移 1 单元（"n"=1 字节）→ 16，绝不能跳回文档开头
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), 15 + 1, "光标应停在组合内相对位置而非文档开头");
    assert_eq!(editor_text(&host, &mut cx), "一二三四五ni");
    // 再输一次组合，替换 marked、不崩溃、光标仍在组合内
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_and_mark_text_in_range(None, "你i", Some(1..1), window, cx);
        });
    });
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), 15 + "你".len(), "光标应停在组合内 '你' 之后");
    assert_eq!(editor_text(&host, &mut cx), "一二三四五你i");
    // 提交
    cx.update(|window, app| {
        ed.update(app, |ed, cx| {
            ed.replace_text_in_range(None, "你", window, cx);
        });
    });
    assert_eq!(editor_text(&host, &mut cx), "一二三四五你");
    let sel = editor_selection(&host, &mut cx);
    assert_eq!(sel.head(), 15 + "你".len());
}

#[gpui::test]
fn cjk_long_line_does_not_narrow_content_width(cx: &mut TestAppContext) {
    // 回归（用户实测）：纯中文长行曾因「字符数×等宽」低估字形宽 → 行宽小于真实
    // 文本宽 → StyledText 行内折行 → 行号错位、点下一行文本重叠。
    // 现在宽度按「宽度单位」计，内容宽必须 ≥ 真实最宽行的整形宽（即不会折行）。
    let cjk = "一".repeat(200); // 200 全角字 = 400 格 = 600 字节
    let (host, mut cx) = open(cx, &format!("{cjk}\n第二行"));
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let content_w = cx.update(|_, cx| ed.read(cx).test_content_w_px());
    // 真实整形宽（行内末字边界 x，byte_col=600 = 行尾）
    let shaped = cx.update(|window, cx| {
        ed.read(cx)
            .test_char_boundary_x(window, 0, 200 * 3)
    });
    assert!(
        content_w >= shaped - 1.0 && content_w >= 400.0 * cw - 1.0,
        "内容宽应 ≥ 真实最宽行（content={content_w:.1} shaped宽={shaped:.1} 400×charw={:.1}）",
        400.0 * cw
    );
}

#[gpui::test]
fn gutter_scroll_locked_to_text_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：滚轮滚动时行号「翻动/闪烁」——旧实现是每次 render 手工按
    // `logical_scroll_top` 重建行号叠层，与 List 行绘制不同步。现在行号列是独立的
    // 第二个 `List`（独立 ListState），由 `sync_gutter_scroll` 与文本区逐帧锁步。
    let text = (1..=100).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let (host, mut cx) = open(cx, &text);
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (t0, g0) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert_eq!(t0, g0, "初始行号应与文本对齐（{t0:?} vs {g0:?}）");
    // 向下滚 6 格（每格 3 行 ≈ 3×行高）
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + 100.),
    );
    for _ in 0..6 {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: pos,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-30.))),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        settle(&mut cx);
    }
    let (t1, g1) = (
        cx.update(|_, cx| ed.read(cx).test_text_scroll_top()),
        cx.update(|_, cx| ed.read(cx).test_gutter_scroll_top()),
    );
    assert!(t1.0 > t0.0, "应已向下滚动（{t0:?} → {t1:?}）");
    assert_eq!(t1, g1, "滚动后行号应与文本锁步一致（{t1:?} vs {g1:?}）");
}

#[gpui::test]
fn hscroll_max_shrinks_after_deleting_widest_content(cx: &mut TestAppContext) {
    // 回归（用户实测）：横向滚动条拉到最右 → 末尾追加超宽内容（量程变大）→
    // 删除后量程不缩回。方案 A+B：删除置脏后（量程可见 + 首次/节流）精确重算使
    // max_line_units 收敛，render 再把 h_handle 夹回新量程——不逐键 O(n)。
    let (host, mut cx) = open(cx, "start\nline2");
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let base_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    // 光标移到文末，追加 600 个 x 的超宽内容
    let len = cx.update(|_, cx| host.read(cx).ed.read(cx).text_string().len());
    ed.update(cx, |ed, _| ed.set_caret_to(len));
    let wide = "x".repeat(600);
    ed.update(cx, |ed, cx| ed.insert_text(&wide, cx));
    settle(&mut cx);
    let wide_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        wide_max > base_max + 1.0,
        "追加超宽内容后量程应增大（{base_max} → {wide_max}）"
    );
    // 撤销 = 删除刚追加的超宽内容 → 量程应缩回
    let w = window_handle(&mut cx);
    cx.dispatch_keystroke(w, Keystroke::parse("ctrl-z").unwrap());
    settle(&mut cx);
    let shrunk_max = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        shrunk_max <= base_max + 1.0,
        "删除超宽内容后量程应缩回（wide={wide_max} → shrunk={shrunk_max}，base={base_max}）"
    );
}

#[gpui::test]
fn click_near_line_start_keeps_horizontal_scroll(cx: &mut TestAppContext) {
    // 回归（用户实测）：点击行首/前几个字符曾把内容强行拉去最左（内容左右晃动）。
    // 根因：鼠标点击也走 `reveal_caret` 的横向 margin 规则（col<margin → hscroll=col）。
    // 现在点击/拖动只做竖向 reveal，横向滚动只能由键盘/输入/滚轮驱动。
    let long = "x".repeat(600);
    let (host, mut cx) = open(cx, &format!("short\n{long}"));
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (_, lh, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    // 先横向滚到右侧
    let ed_b = cx.debug_bounds("code-editor").expect("编辑器应渲染");
    let pos = point(
        px(ed_b.origin.x.as_f32() + 200.),
        px(ed_b.origin.y.as_f32() + lh * 0.5),
    );
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: pos,
        delta: gpui::ScrollDelta::Pixels(point(px(400.), px(0.))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    settle(&mut cx);
    let before = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(before > 0., "应先横向滚动（before={before}）");
    // 点击短行行首（空白处/首个字符）——不得把内容拖回最左
    let y = px(lh * 0.5);
    let x = px(gutter + 2.0);
    let mods = Modifiers::default();
    cx.simulate_mouse_move(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, y), MouseButton::Left, mods);
    settle(&mut cx);
    let after = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after.abs() >= before - 0.1,
        "点击不应重置横向滚动（before={before} → after={after}）"
    );
    // 空白行同样不应重置
    let blank_y = px(lh * 2.5);
    cx.simulate_mouse_move(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_down(point(x, blank_y), MouseButton::Left, mods);
    cx.simulate_mouse_up(point(x, blank_y), MouseButton::Left, mods);
    settle(&mut cx);
    let after2 = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(
        after2.abs() >= before - 0.1,
        "点击行首空白也不应重置横向滚动（{before} → {after2}）"
    );
}

#[gpui::test]
fn typing_long_cjk_keeps_caret_visible(cx: &mut TestAppContext) {
    // 回归（用户实测）：内容超出窗口后，新输入不可见（光标没跟随）。
    // 根因：横向 reveal 用「字符列」，中文每字占 2 格 → 少滚一半。改按「宽度单位」
    // 列后，中文长行光标会滚回可视区。
    let (host, mut cx) = open(cx, "");
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, _gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let visible = cx.update(|_, cx| ed.read(cx).test_visible_chars());
    // 打远超可视格的中文（400 字 = 800 格）
    cx.simulate_keystrokes(&"你".repeat(400));
    settle(&mut cx);
    let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
    assert!(hscroll > 0., "超窗输入应自动横向滚到光标处（hscroll={hscroll}）");
    // 光标近似 x（单位列 800 格）× 格宽 − 已滚像素应落在可视区 [0, 可视格×格宽]
    let caret_x_approx = 800.0 * cw;
    let visible_px = (visible.saturating_sub(4)) as f32 * cw;
    assert!(
        caret_x_approx - hscroll <= visible_px + cw * 2.0
            && caret_x_approx - hscroll + cw >= 0.0,
        "光标应滚回可视区（caret_x={caret_x_approx:.0} hscroll={hscroll:.0} 可视≈{visible_px:.0}）"
    );
}

#[gpui::test]
fn hscroll_bar_max_tracks_content_growth(cx: &mut TestAppContext) {
    // 回归（用户实测）：底部横向滚动条不随内容变化。量程应跟随 content_w_px 增长。
    let (host, mut cx) = open(cx, "");
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let max0 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(max0 == 0., "短内容量程应为 0（max0={max0}）");
    // 输入超过可视宽度 → content_w_px 增长 → 量程应 > 0
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(&mut cx);
    let max1 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max1 > 0.,
        "内容变宽后横向滚动条量程应 > 0（max1={max1}）"
    );
    // 追加更长 → 量程继续增长
    cx.simulate_keystrokes(&"x".repeat(400));
    settle(&mut cx);
    let max2 = cx.update(|_, cx| ed.read(cx).test_hscroll_max_px());
    assert!(
        max2 > max1 + 1.0,
        "更宽内容应让量程继续增长（{max1} → {max2}）"
    );
}

#[gpui::test]
fn vertical_range_stays_healthy_while_typing_in_widest_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：在最宽长行末尾持续输入时，纵向滚动条“突然变长又变短”。
    // 根因：内容宽逐键变化 → gpui List 布局宽度变化时清空全部 size_hint → 纵向量程
    // 坍缩；`ensure_uniform_hint` 的 120ms 冷却阻止持续打字时自愈（量程一直缩成极小）。
    // 修复：内容宽按 64px 向上量化，只在每 ~8 字符才变化一次，冷却期间即可自愈。
    let mut text = String::from("x".repeat(10000));
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, mut cx) = open(cx, &text);
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let healthy = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(healthy > 6000., "初始纵向量程应健康（{healthy}）");
    // 光标移到最宽行（第 1 行）末尾
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    // 持续输入 80 字符（跨多个 64px 桶）
    for _ in 0..80 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(&mut cx);
    }
    // 补一拍冷却，确认自愈而非一直坍缩
    std::thread::sleep(std::time::Duration::from_millis(150));
    settle(&mut cx);
    let after = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
    assert!(after > 6000., "持续输入后纵向量程应保持健康（{after}）");
}

#[gpui::test]
fn reveal_keeps_caret_visible_while_typing_in_long_line(cx: &mut TestAppContext) {
    // 回归（用户实测）：长行末尾持续输入时「横向滚动条算错，最新内容看不见」。
    // 根因：`reveal_caret` 把带 +8 估算余量的 visible_chars 当真实可视格数 → 光标被
    // 滚到真实右缘之外 ~4 格（实测 screen=1911 > 视口1883）。修复：先减掉 +8 余量。
    let mut text = String::from("x".repeat(10000));
    for i in 1..=500 {
        text.push_str(&format!("\nline{i}"));
    }
    let (host, mut cx) = open(cx, &text);
    focus(&host, cx);
    settle(&mut cx);
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    let (cw, _, gutter) = cx.update(|_, cx| ed.read(cx).test_metrics());
    let viewport_px = 1920.0 - gutter; // 测试窗 1920 宽
    ed.update(cx, |ed, _| ed.set_caret_to(10000));
    for _ in 0..60 {
        ed.update(cx, |ed, cx| ed.insert_text("x", cx));
        settle(&mut cx);
        let hscroll = cx.update(|_, cx| ed.read(cx).test_hscroll_px());
        let caret_units = cx.update(|_, cx| host.read(cx).ed.read(cx).test_caret_units());
        let screen = caret_units as f32 * cw - hscroll;
        assert!(
            (0.0..=viewport_px + 0.5).contains(&screen),
            "光标应滚回可视区（screen={screen:.0} ≤ 视口{viewport_px:.0}）"
        );
    }
}

#[gpui::test]
fn scroll_range_survives_width_change(cx: &mut TestAppContext) {
    // 回归（2026-09-15 实机）：gpui 在宽度变化时清空全部 size_hint，
    // 未测条目高度按 0 计 → `max_offset_for_scrollbar()` 坍缩，滚动条量程失真。
    // 编辑器应在坍缩后重挂统一高度提示，量程不被换宽清掉。
    //
    // 注：横向内容宽度跟随窗口宽（短文档 content=avail_w），短文档每次换宽都会
    // 触发 List 重测；`ensure_uniform_hint` 的 120ms 风暴冷却（防拖分屏每帧 O(n)）
    // 用真实墙钟计。因此两次 resize 之间要留出冷却窗口，模拟真实人类换窗节奏
    // （0.12s 内的连发 resize 属于“拖拽风暴”，量程暂时性失真本就是冷却的取舍）。
    let text = (1..=300)
        .map(|i| format!("第{i}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let (host, mut cx) = open(cx, &text);
    // 横向内容宽度跟随窗口宽（短文档 content=avail_w），换宽会让 gpui List 在
    // prepaint 里清空全部 size_hint → 量程坍缩；`ensure_uniform_hint` 重挂统一高度
    // 会自愈，但其 120ms 风暴冷却（防拖分屏每帧 O(n)）用真实墙钟计，且换宽那一帧
    // 的 heand 会被同帧 prepaint 再次抹掉。因此每轮换宽后以「等冷却→重渲染」循环
    // 直到量程恢复，断言的是「换宽后量程最终自愈」而非瞬时（连发 resize 属拖拽风暴，
    // 瞬时失真本就是冷却的取舍）。
    fn reflow_and_assert(host: &Entity<Host>, cx: &mut VisualTestContext, w: f32) -> f32 {
        cx.simulate_resize(size(px(w), px(600.)));
        let mut m = 0.0;
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            settle(cx);
            m = cx.update(|_, cx| host.read(cx).ed.read(cx).list_max_offset());
            if m > 1000. {
                break;
            }
        }
        assert!(m > 1000., "换宽后量程应在冷却后自愈（m={m:.1} w={w}）");
        m
    }
    let mut last_max = reflow_and_assert(&host, &mut cx, 300.);
    assert!(
        last_max > 1000.,
        "首次渲染量程应接近 300 行（last_max={last_max:.1}）"
    );
    // 再换两次宽 → 应自愈，量程保持。
    for w in [440., 520.] {
        last_max = reflow_and_assert(&host, &mut cx, w);
    }
    let lh = cx.update(|_, cx| host.read(cx).ed.read(cx).test_metrics().1);
    let pad = cx.update(|_, cx| host.read(cx).ed.read(cx).test_bottom_pad_px());
    assert!(
        (last_max - (300.0 * lh + pad - 600.0)).abs() < lh * 4.,
        "量程应≈300×行高+底部预留−视口高（{last_max:.1} vs {:.1}）",
        300.0 * lh + pad - 600.0
    );
}

// ---------------------------------------------------------------------------
// 粘贴/列表窗口（回归：空标签粘贴只显示首行）
// ---------------------------------------------------------------------------

#[gpui::test]
fn empty_new_tab_paste_not_collapsed_to_first_row(cx: &mut TestAppContext) {
    // 回归（用户实测「新建空标签 → 粘贴多行 → 编辑器只显示第一行」）：
    // `CodeEditor::new` 建空标签时 ListState 若按 `row_count`(1) 初始化，则固定窗口
    // 虚拟化从未建立（`resync_list` 只在 set_text/开文件/撤销重做时重置为
    // `LIST_WINDOW_ROWS`）；`apply_desired_to_window` 的「窗口覆盖」判定又会误判
    // 无需补建 → 粘贴 300 行后列表仍只有 1 项 → `list()` 只渲染首行。
    // 修复：`new()` 直接以固定窗口 `LIST_WINDOW_ROWS` 初始化。
    use crate::config::limits::EDITOR_LIST_PROVISIONAL_ROWS as WINDOW;
    cx.update(gpui_component::init);
    let (host, cx) = cx.add_window_view(|_window, cx| {
        let ed = cx.new(|cx| CodeEditor::new(cx));
        Host { ed }
    });
    let cx: &mut VisualTestContext = cx;
    settle(cx);
    let win0 = cx.update(|_, cx| host.read(cx).ed.read(cx).test_list_window_len());
    assert_eq!(
        win0, WINDOW,
        "空标签首帧 ListState 就应为固定窗口 {WINDOW} 项（而不是 1 项）"
    );
    // 粘贴 300 行多行文本。
    let text = (0..300).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
    let ed = cx.update(|_, cx| host.read(cx).ed.clone());
    ed.update(cx, |ed, cx| ed.paste_text(&text, cx));
    settle(cx);
    let (lc, vis, win, sic) = cx.update(|_, cx| {
        let ed = host.read(cx).ed.read(cx);
        (
            ed.line_count(),
            ed.test_folds().visual_line_count(),
            ed.test_list_window_len(),
            ed.test_scroll_item_count(),
        )
    });
    assert_eq!(lc, 300, "数据行数应完整");
    assert_eq!(vis, 300, "视觉行数应完整");
    assert!(sic >= vis, "滚动量程应覆盖全部视觉行（sic={sic} vis={vis}）");
    assert_eq!(
        win, WINDOW,
        "粘贴后固定窗口条目数应保持 = {WINDOW}（实际 {win}）——否则只渲染首行"
    );
}

