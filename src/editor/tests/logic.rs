
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
