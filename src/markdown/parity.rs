//! 奇偶（parity）测试：钉死「预览（`parser` 行扫描）与 WYSIWYG（`live` 的
//! `markdown_syntax` 行判定）」两套 Markdown 理解层对同一段源码的判定一致。
//!
//! 目标**不是两边全等**（taxonomy 本就不同：live 多 Todo/Callout/Math/折叠，
//! 预览多 Rule/Blank/Image/EPUB 对齐指令），而是：
//! - 公共构造（标题 / 列表(含 todo) / 引述 / 分隔线 / 独立图片行 / 表格 cells）逐行一致；
//! - 已知差异：用 [`known_differences_are_documented`] **显式断言固化**，防止未来某一边
//!   悄悄改变行为而无人察觉。
//!
//! 这是「两套融合」路线 2 的地基：先证明两边已一致，阶段 2 才允许去重。
//! 任何一边改动这些公共识别规则，本文件应立即红掉。

#![cfg(test)]

use std::collections::HashMap;

use gpui::SharedString;

use super::live::markdown_syntax::{
    alert_kind, blockquote_prefix, heading_level, image_row, is_table_row, list_prefix,
    math_regions, table_cells, table_regions, task_prefix, thematic_break,
};
use super::parser::{parse_to_rows, split_table_cells, RowKind, TableAlign};

/// 复刻 live 的围栏奇偶规则（`scan_data` 里的 4 行循环，live.rs:1688-1695）：
/// 行首（允许缩进）以 ` ``` ` 开头则翻转；`odd` 表示「该行起点已在围栏内部」。
/// 仅测试对照用，不参与实现。
fn live_fence_odd(src: &str) -> Vec<bool> {
    let mut odd = false;
    src.split('\n')
        .map(|l| {
            let cur = odd;
            if l.trim_start().starts_with("```") {
                odd = !odd;
            }
            cur
        })
        .collect()
}

#[test]
fn headings_agree_on_atx() {
    let src = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\nplain\n";
    let rows = parse_to_rows(src);
    let by_line: HashMap<usize, u8> = rows
        .iter()
        .filter_map(|r| match r.kind {
            RowKind::Heading(n) => Some((r.src_line, n)),
            _ => None,
        })
        .collect();
    for (i, line) in src.split('\n').enumerate() {
        match (heading_level(line), by_line.get(&i)) {
            (Some(a), Some(&b)) => assert_eq!(a, b, "line {i}: `{line}`"),
            (None, None) => {}
            (a, b) => panic!("line {i} `{line}`: live={a:?} parser={b:?}"),
        }
    }
}

#[test]
fn lists_and_tasks_agree() {
    let src = "- alpha\n* beta\n+ gamma\n1. first\n2) second\n- [ ] todo\n- [x] done\nplain\n";
    let rows = parse_to_rows(src);
    let bullets: HashMap<usize, &str> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Bullet)
        .map(|r| (r.src_line, r.prefix.as_deref().unwrap_or("")))
        .collect();
    for (i, line) in src.split('\n').enumerate() {
        let is_list = list_prefix(line).is_some();
        let is_parser = bullets.contains_key(&i);
        assert_eq!(is_list, is_parser, "line {i} `{line}`");
        if is_list {
            let is_task = task_prefix(line).is_some();
            let p = bullets[&i];
            let parser_task = p.contains('☐') || p.contains('☑');
            assert_eq!(is_task, parser_task, "task line {i} `{line}` (prefix `{p}`)");
        }
    }
}

#[test]
fn quotes_and_callouts_agree() {
    let src = "> a quote\n> [!NOTE]\n> callout body\nplain\n";
    let rows = parse_to_rows(src);
    let quotes: Vec<usize> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Quote)
        .map(|r| r.src_line)
        .collect();
    for (i, line) in src.split('\n').enumerate() {
        let is_quote = blockquote_prefix(line).is_some();
        assert_eq!(is_quote, quotes.contains(&i), "line {i} `{line}`");
        if is_quote {
            // live 在 Quote 内再细分 Callout；预览仍按 Quote 渲染。
            let p = blockquote_prefix(line).unwrap_or(0);
            let is_callout = alert_kind(&line[p..]).is_some();
            if i == 1 {
                assert!(is_callout, "line 1 应为 callout，live 只在引述基础上再加细分");
            }
        }
    }
}

#[test]
fn rules_and_images_agree() {
    let src = "---\n***\n___\n![alt](img.png)\nplain\n";
    let rows = parse_to_rows(src);
    let rules: Vec<usize> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Rule)
        .map(|r| r.src_line)
        .collect();
    let images: Vec<usize> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Image)
        .map(|r| r.src_line)
        .collect();
    for (i, line) in src.split('\n').enumerate() {
        assert_eq!(
            thematic_break(line),
            rules.contains(&i),
            "rule line {i} `{line}`"
        );
        assert_eq!(
            image_row(line).is_some(),
            images.contains(&i),
            "image line {i} `{line}`"
        );
    }
}

#[test]
fn tables_cells_agree() {
    let src = "| 型号 | 尺寸 |\n| :---: | ---: |\n| A | 10 |\n| B | 20 |\n";
    let rows = parse_to_rows(src);
    let table = rows
        .iter()
        .find(|r| r.kind == RowKind::Table)
        .expect("应解析出一张表格");
    assert_eq!(table.src_line, 0);
    let td = table.table().unwrap();
    assert_eq!(
        td.header.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
        vec!["型号", "尺寸"]
    );
    assert_eq!(
        td.aligns,
        vec![TableAlign::Center, TableAlign::Right],
        "分隔行 `:---:| ---:` 两边都应是 Center/Right"
    );

    // live 侧区域
    let regions = table_regions(src);
    assert_eq!(regions.len(), 1);
    let region = &regions[0];
    assert_eq!(region.lines.start, table.src_line);
    // 4 行表区域（表头+分隔+2 数据行）；src 末带换行，split 会多一个空段。
    assert_eq!(region.lines.end, 4);

    // 每个真实行（表头/分隔/数据）的 cells：本语料无转义竖线，两边应逐格一致。
    for line in src.split('\n') {
        if !is_table_row(line) {
            continue;
        }
        let lc: Vec<&str> = table_cells(line);
        let pc: Vec<SharedString> = split_table_cells(line);
        assert_eq!(lc.len(), pc.len(), "cell 数 on `{line}`");
        for (a, b) in lc.iter().zip(pc.iter()) {
            assert_eq!(*a, b.as_ref(), "cell on `{line}`");
        }
    }
}

#[test]
fn known_differences_are_documented() {
    // 1) `~~~` 围栏：预览识别为 Code；live 只认 ` ``` `，`~~~` 完全不进围栏。
    let tildes = "~~~\nx\n~~~\n";
    let rows = parse_to_rows(tildes);
    assert_eq!(rows[0].kind, RowKind::Code);
    assert!(live_fence_odd(tildes).iter().all(|&b| !b));

    // 2) 无空格 ATX：`#abc` 预览算标题，live 不算（live 要求 `# ` 或行尾）。
    assert_eq!(parse_to_rows("#abc\n")[0].kind, RowKind::Heading(1));
    assert_eq!(heading_level("#abc"), None);

    // 3) 裸 `###`（无内容）：live 算 3 级标题；预览因无内容落到段落。
    assert_eq!(heading_level("###"), Some(3));
    assert_eq!(parse_to_rows("###\n")[0].kind, RowKind::Paragraph);

    // 4) 缩进引述 `  > x`：预览是 Quote（先 trim_start）；live 要求 `>` 在字节 0。
    assert_eq!(parse_to_rows("  > x\n")[0].kind, RowKind::Quote);
    assert_eq!(blockquote_prefix("  > x"), None);

    // 5) 空格分隔线 `- - -`：预览在前置列表分支先命中 → Bullet；live 判 thematic_break。
    assert_eq!(parse_to_rows("- - -\n")[0].kind, RowKind::Bullet);
    assert!(thematic_break("- - -"));

    // 6) `$$…$$` math 围栏：live 有 math 区域；预览无 math 概念（`$$` 行 → 段落）。
    assert_eq!(math_regions("$$\nx\n$$\n").len(), 1);
    assert_eq!(parse_to_rows("$$\n")[0].kind, RowKind::Paragraph);

    // 7) 表格转义竖线：预览解义 `\|` → `|`（显示）；live 保留原样（源码逐字节，光标/IME 映射用）。
    assert_eq!(split_table_cells("1 | x\\|y\n")[1].as_ref(), "x|y");
    assert_eq!(table_cells("1 | x\\|y\n")[1], "x\\|y");

    // 8) 无外竖线表头 `a | b`：预览按表头解析成表格；live `is_table_row` 要求行首 `|`。
    let no_outer = "a | b\n- | -\n1 | 2\n";
    assert_eq!(parse_to_rows(no_outer)[0].kind, RowKind::Table);
    assert!(!is_table_row("a | b"));

    // 9) 图片 width 属性 `{width=N}`：live 识别并返回宽度；预览不识 width，
    //    `![a](x.png){width=50}` 带尾文字 → 不算独立图片行（落到段落）。
    let w = "![a](x.png){width=50}";
    assert!(image_row(w).is_some());
    assert_ne!(parse_to_rows(w)[0].kind, RowKind::Image);
}
