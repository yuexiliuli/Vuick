//! 奇偶（parity）测试：钉死「预览（`parser` 行扫描）与 WYSIWYG（`live` 的
//! `inline_style` 行判定）」两套 Markdown 理解层对同一段源码的判定一致。
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

use std::collections::{HashMap, HashSet};

use gpui::SharedString;

use crate::markdown::ir;
use super::inline_style::{
    alert_prefix, blockquote_prefix, heading_level, image_row, is_table_row, list_prefix,
    math_regions, table_cells, table_regions, task_prefix, thematic_break,
};
use super::rows::{parse_to_rows, split_table_cells, RowKind, TableAlign};

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
            let is_callout = alert_prefix(&line[p..]).is_some();
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

    // 10) 缩进标题 `  # x`：预览先 trim_start → Heading；live `heading_level` 要求
    //     `#` 在字节 0 → 段落（除非跟在列表标记后）。
    assert_eq!(parse_to_rows("  # x\n")[0].kind, RowKind::Heading(1));
    assert_eq!(heading_level("  # x"), None);
}

#[test]
fn image_construct_edges_are_documented() {
    // `![a]b](c)`：预览 alt 由「第一个 `]`」界定且必须紧跟 `(` → 整行不算图片；
    // live 取「第一处 `](`」→ 识别成功（src="c"）。
    let weird = "![a]b](c)";
    assert_eq!(parse_to_rows(weird)[0].kind, RowKind::Paragraph);
    assert!(image_row(weird).is_some());

    // `![alt] (src)`：`]` 与 `(` 之间有空格 → 两边都不识别为图片。
    let spaced = "![a] (b)";
    assert_ne!(parse_to_rows(spaced)[0].kind, RowKind::Image);
    assert!(image_row(spaced).is_none());

    // 列表项图片 `- ![x](y)`：live `image_row` 识别（列表标记 + 图片）；预览列表
    // 分支先命中 → Bullet，不是 Image。
    let li_img = "- ![x](y)";
    assert_eq!(parse_to_rows(li_img)[0].kind, RowKind::Bullet);
    assert!(image_row(li_img).is_some());
}

/// 逐行「粗粒度 kind」，供整文档一致性对照（对齐子集语料）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Paragraph,
    Heading(u8),
    Bullet,
    Quote,
    Rule,
    Image,
    Blank,
    Code,
    Table,
}

impl Tag {
    fn from_row(kind: RowKind) -> Option<Tag> {
        Some(match kind {
            RowKind::Paragraph => Tag::Paragraph,
            RowKind::Heading(n) => Tag::Heading(n),
            RowKind::Bullet => Tag::Bullet,
            RowKind::Quote => Tag::Quote,
            RowKind::Rule => Tag::Rule,
            RowKind::Image => Tag::Image,
            RowKind::Blank => Tag::Blank,
            RowKind::Code | RowKind::Table => return None, // 多行块，按 span 展开
        })
    }
}

/// 预览侧每行 kind（多行块 Code/Table 按 live 区域展开成逐行）。
fn preview_line_tags(src: &str) -> Vec<Tag> {
    let n = src.split('\n').count();
    let mut tags = vec![Tag::Paragraph; n];
    let regions = table_regions(src);
    for r in parse_to_rows(src) {
        let span: std::ops::Range<usize> = match r.kind {
            RowKind::Table => regions
                .iter()
                .find(|rg| rg.lines.start == r.src_line)
                .map(|rg| rg.lines.clone())
                .unwrap_or(r.src_line..n),
            RowKind::Code => {
                let lines: Vec<&str> = src.split('\n').collect();
                let mut end = r.src_line + 1;
                while end < lines.len() && !lines[end].trim_start().starts_with("```") {
                    end += 1;
                }
                let end = if end < lines.len() { end + 1 } else { n };
                r.src_line..end
            }
            _ => r.src_line..r.src_line + 1,
        };
        let tag = match r.kind {
            RowKind::Code => Tag::Code,
            RowKind::Table => Tag::Table,
            _ => Tag::from_row(r.kind).expect("单行块必有 kind"),
        };
        for i in span {
            tags[i] = tag;
        }
    }
    tags
}

/// live 侧每行 kind。
fn live_line_tags(src: &str) -> Vec<Tag> {
    let lines: Vec<&str> = src.split('\n').collect();
    let fence = live_fence_odd(src);
    let table_lines: HashSet<usize> =
        table_regions(src).into_iter().flat_map(|rg| rg.lines).collect();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if table_lines.contains(&i) {
                return Tag::Table;
            }
            if fence.get(i).copied().unwrap_or(false) || l.trim_start().starts_with("```") {
                return Tag::Code;
            }
            if let Some(h) = heading_level(l) {
                return Tag::Heading(h);
            }
            if blockquote_prefix(l).is_some() {
                return Tag::Quote;
            }
            if list_prefix(l).is_some() {
                return Tag::Bullet;
            }
            if thematic_break(l) {
                return Tag::Rule;
            }
            if image_row(l).is_some() {
                return Tag::Image;
            }
            if l.trim().is_empty() {
                return Tag::Blank;
            }
            Tag::Paragraph
        })
        .collect()
}

/// IR 侧每行 kind。
fn ir_line_tags(src: &str) -> Vec<Tag> {
    let n = src.split('\n').count();
    let mut tags = vec![Tag::Paragraph; n];
    for b in ir::build_ir(src) {
        let tag = match b.kind {
            ir::IrKind::Paragraph => Tag::Paragraph,
            ir::IrKind::Heading(h) => Tag::Heading(h),
            ir::IrKind::Bullet => Tag::Bullet,
            ir::IrKind::Quote => Tag::Quote,
            ir::IrKind::Rule => Tag::Rule,
            ir::IrKind::Image => Tag::Image,
            ir::IrKind::Blank => Tag::Blank,
            ir::IrKind::Code => Tag::Code,
            ir::IrKind::Table => Tag::Table,
        };
        for i in b.lines.clone() {
            if i < n {
                tags[i] = tag;
            }
        }
    }
    tags
}

/// 对齐子集语料上的「预览 == live == IR」三向逐行一致。
#[test]
fn per_line_kinds_agree_across_rich_docs() {
    let docs = [
        "下\n\n# 标题\n一些正文\n- 列表\n  - 嵌套\n1. 有序\n- [ ] 待办\n> 引述\n---\n\n```rust\nlet a = 1;\n```\n\n| 列A | 列B |\n| :- | -: |\n| 1 | 2 |\n\n![图](a.png)\n*斜体* 与 `代码`\nשלום עולם\n> 引述2\n- 收尾\n",
        "# 章节\n\n段落一 内容\n段落二 **加粗** 与 文本\n\n## 小节\n- [x] 已完成\n- [ ] 待办\n  继续\n1) 数字\n\n> 块引用\n\n```\nconst x: number = 1;\n```\n\n---\n![logo](logo.png)\n",
    ];
    for (k, src) in docs.iter().enumerate() {
        let pv = preview_line_tags(src);
        let lv = live_line_tags(src);
        let ir = ir_line_tags(src);
        assert_eq!(pv, lv, "doc {k}: 预览 vs live 逐行不一致");
        assert_eq!(ir, lv, "doc {k}: IR 与 live 不符（IR 是金标准）");
        assert_eq!(ir, pv, "doc {k}: IR 与预览不符（IR 是金标准）");
    }
}

/// IR 的结构化语义（表格 cells/对齐、代码语言）同时对齐 live 与预览。
#[test]
fn ir_carries_table_structure_and_code_language() {
    let src = "| 型号 | 尺寸 |\n| :- | -: |\n| A | 10 |\n| B | 20 |\n";
    let blocks = ir::build_ir(src);
    let tbl = blocks
        .iter()
        .find(|b| b.kind == ir::IrKind::Table)
        .expect("应为表格块");
    let t = tbl.table.as_ref().expect("表格块应带结构");
    assert_eq!(t.header, vec!["型号".to_string(), "尺寸".to_string()]);
    assert_eq!(
        t.rows,
        vec![
            vec!["A".to_string(), "10".to_string()],
            vec!["B".to_string(), "20".to_string()]
        ]
    );
    assert_eq!(
        t.aligns,
        vec![super::cells::CellAlign::Left, super::cells::CellAlign::Right],
        "分隔行 `:-| -:` 都是 Left/Right（与 parser、live 同一对齐实现）"
    );

    // IR 结构 == live `table_cells` / parser `split_table_cells`（无转义语料）逐格一致。
    for line in src.split('\n') {
        if line.trim().is_empty() {
            continue; // 空行不属于表格；parser 丢空 cell、live 保留，非表格语义
        }
        let lc: Vec<String> = table_cells(line).into_iter().map(str::to_string).collect();
        let pc: Vec<String> = split_table_cells(line)
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect();
        let rc = super::cells::raw_cells(line);
        assert_eq!(rc, lc, "IR 与 live 单元格 line `{line}`");
        assert_eq!(rc, pc, "IR 与 parser 单元格 line `{line}`");
    }

    // Code 语言：IR == parser。
    let code_src = "```rust\nlet a = 1;\n```\n";
    let code = ir::build_ir(code_src)
        .into_iter()
        .find(|b| b.kind == ir::IrKind::Code)
        .unwrap();
    assert_eq!(code.code_language.as_deref(), Some("rust"));
    assert_eq!(parse_to_rows(code_src)[0].code_language(), Some("rust"));
}

/// 底座 `Doc`（统一文档模型）与两分支一致：DocRow.kind == IR、text == 预览、逐行覆盖。
#[test]
fn doc_model_agrees_with_preview_and_ir() {
    let src = "# 标题\n正文 **加粗** 与 `代码`\n\n- 列表\n  - 嵌套\n1) 数字\n- [ ] 待办\n> 引述\n> [!NOTE]\n---\n```rust\nlet a = 1;\nlet b = 2;\n```\n\n| 列A | 列B |\n| :- | -: |\n| 1 | 2 |\n\n![图](a.png)\nשלום עולם\n\n- 收尾\n";
    let n_lines = src.split('\n').count();

    // Doc 全量物化：kind 与 IR 逐行一致、text 与预览 parse_to_rows 逐块一致。
    let doc = crate::markdown::doc::Doc::open(SharedString::from(src));
    let rows = doc.rows_in_line_window(0, n_lines);
    let ir_blocks = ir::build_ir(src);
    for r in &rows {
        for line in r.lines.clone() {
            let ir_kind = ir_blocks
                .iter()
                .find(|b| b.lines.contains(&line))
                .map(|b| b.kind)
                .expect("IR 应有该行");
            assert_eq!(r.kind, ir_kind, "line {line} 语义不一致");
        }
    }
    // text 与预览一致：对每块的 src_line，parse_to_rows 同 src_line 的 Row 文本相同。
    let pv: std::collections::HashMap<usize, String> = parse_to_rows(src)
        .iter()
        .map(|r| (r.src_line, r.text.as_ref().to_string()))
        .collect();
    for r in &rows {
        let expect = pv.get(&r.src_line).expect("预览应有该块");
        assert_eq!(r.text.as_ref(), expect.as_str(), "src_line {} 文本不一致", r.src_line);
    }

    // 块粒度：DocRow 覆盖的源码行区间不重叠、不遗漏（= 全文档行集）。
    let mut covered: Vec<usize> = rows.iter().flat_map(|r| r.lines.clone()).collect();
    covered.sort_unstable();
    let missing: Vec<usize> = (0..n_lines).filter(|i| !covered.contains(i)).collect();
    assert!(missing.is_empty(), "Doc 未覆盖行: {missing:?}");
}

/// 底座双源路径一致：Rope（大文档 COW 路径）窗口行 == Str（小文档路径）逐块相同。
/// 这是「preview 用 Rope 免整份拷贝、live 将来同路径」的性能底座正确性护栏。
#[test]
fn doc_rope_path_matches_str_path() {
    let src = "# 标题\n正文 **加粗** `代码`\n\n- 列表\n  - 嵌套\n1) 数字\n> 引述\n---\n```rust\nlet a = 1;\n```\n\n| A | B |\n| - | - |\n| 1 | 2 |\n\n![图](a.png)\n";
    let n_lines = src.split('\n').count();

    let str_doc = crate::markdown::doc::Doc::open(SharedString::from(src));
    let rope = ropey::Rope::from(src);
    let rope_doc = crate::markdown::doc::Doc::open_rope(rope);

    assert_eq!(str_doc.block_count(), rope_doc.block_count(), "两源块数应一致");
    let sc = str_doc.rows_in_line_window(0, n_lines);
    let rc = rope_doc.rows_in_line_window(0, n_lines);
    assert_eq!(sc.len(), rc.len(), "两源窗口行数一致");
    for (a, b) in sc.iter().zip(rc.iter()) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.src_line, b.src_line);
        assert_eq!(a.text, b.text, "src_line {} 文本不一致（Rope vs Str）", a.src_line);
        assert_eq!(a.lines, b.lines);
        assert_eq!(a.byte_map, b.byte_map, "src_line {} 映射不一致", a.src_line);
        assert_eq!(a.payload, b.payload, "src_line {} 负载不一致（Rope vs Str）", a.src_line);
    }
}

/// 底座编辑映射（渲染字节→源字节）与预览同段派生：表头/序号/加粗被剥后仍能
/// 把「渲染位置」换算回「该行源字节」——live 点击/光标/IME 落点所要的原始数据。
#[test]
fn doc_byte_map_maps_render_back_to_line_source() {
    // 语料不含尾随换行：行数与 `rows_in_line_window` 的窗口一一对应。
    let src = "标题\n前 **加粗** 后\n- 任务 **粗体** 尾\n> 引用 `代码`";
    let doc = crate::markdown::doc::Doc::open(SharedString::from(src));
    let rows = doc.rows_in_line_window(0, 4);

    let para = &rows[1]; // 行 0="标题"；行 1="前 **加粗** 后"
    assert_eq!(para.text.as_ref(), "前 加粗 后");
    assert_eq!(para.byte_map.len(), para.text.len() + 1, "哨兵 = 渲染文本长+1");
    // "加"（渲染字节 4..）→ 源字节 6（跳过 `**`）：
    let rb = para.text.as_ref().find('加').expect("渲染文本含 加");
    let sb = para.byte_map[rb];
    let line = "前 **加粗** 后";
    assert_eq!(&line[sb..sb + 3], "加", "渲染 加 应落回源行的 加（byte {sb}）");

    // 列表行：`- ` 前缀也计入行内偏移（渲染 任务=byte2..，源 `- ` 后）。
    let bullet = &rows[2];
    assert_eq!(bullet.text.as_ref(), "任务 粗体 尾");
    let r7 = bullet.text.as_ref().find('粗').unwrap();
    let s7 = bullet.byte_map[r7];
    assert_eq!(&src.split('\n').collect::<Vec<_>>()[2][s7..s7 + 3], "粗");

    // 引述行：`> ` 剥掉，映射关照引号后的内容。
    let quote = &rows[3];
    assert_eq!(quote.text.as_ref(), "引用 代码");
    let rq = quote.text.as_ref().find('代').unwrap();
    let sq = quote.byte_map[rq];
    assert_eq!(&src.split('\n').collect::<Vec<_>>()[3][sq..sq + 3], "代");
}

/// 窗口物化一致性回归：对 CRLF、围栏/表格/头部/引用/图片、**尾随空行**混合的文档
/// 穷举所有窗口 `[lo..=hi]`，Str 与 Rope 两个源都必须行数 == 窗口块数。
///
/// 历史事故：`parse_block_window` 只在 `end < src.len()` 时裁尾 '\n'，文档以
/// `\r?\n` 结尾时——窗口收束在最后一个真实文本块（EOF 幻影空行块在其后）→ 切片
/// 多留 1 个换行 → `split('\n')` 多造 1 行 → 调试断言 `70 vs 69` 应用运行时崩溃
/// （`fixtures/测试.md` 打开即死）。修复：末块若不是 EOF 幻影空行块（`byte==len`）
/// 也要裁尾 '\n'；全档窗口（末块=幻影）保留该 '\n'（它就是幻影块本体）。
#[test]
fn window_rows_match_blocks_for_all_windows() {
    use crate::markdown::doc::Doc;
    use gpui::SharedString;
    use ropey::Rope;
    let content = "# 标题\r\n正文 **粗**\r\n\r\n```rust\r\nlet a = 1;\r\nlet b = 2;\r\n```\r\n\r\n| A | B |\r\n|---|---|\r\n| 1 | 2 |\r\n\r\n> 引用\r\n\r\n![图](x.png)\r\n图片后段落\r\n";
    let str_doc = Doc::open(SharedString::from(content));
    let rope_doc = Doc::open_rope(Rope::from(content));
    let n = str_doc.block_count();
    assert_eq!(n, rope_doc.block_count(), "Str/Rope 块数一致");
    for lo in 0..n {
        for hi in lo..n {
            let want = hi - lo + 1;
            let sc = str_doc.rows_in_block_window(lo, hi);
            let rc = rope_doc.rows_in_block_window(lo, hi);
            assert_eq!(sc.len(), rc.len(), "lo={lo} hi={hi} Str/Rope 窗行数一致");
            assert_eq!(
                sc.len(),
                want,
                "lo={lo} hi={hi} Str 窗口行数应等于块数"
            );
            for (a, b) in sc.iter().zip(rc.iter()) {
                assert_eq!(a.kind, b.kind);
                assert_eq!(a.text, b.text, "lo={lo} hi={hi} 文本一致");
            }
        }
    }
}

/// 增量 `line_count/char_count` 与全文重算逐值一致（状态栏显示口径钉死）：
/// `line_count` == `str::lines().count().max(1)`、`char_count` == `str::chars().count()`，
/// 且编辑（插入/删除/跨行）后仍保持——宿主镜像已改为 O(1) 直读这两个计数，错了
/// 状态栏行数/字数就失真。
#[test]
fn doc_incremental_counts_match_full_recompute() {
    use crate::markdown::doc::Doc;
    use gpui::SharedString;

    let docs = [
        "",                              // 空
        "a",                             // 无换行、无尾随换行
        "a\n",                           // 单行尾随换行（幻影尾行）
        "a\nb",                          // 多行无尾随换行
        "a\nb\n",                        // 多行尾随换行
        "a\nb\nc\n\n",                   // 连续尾随换行（parity 尾部语义）
        "# 标题\n正文 **粗**\n- 项\n\n> 引\n```\nlet x;\n```\n\n中文字符与 emoji: 你好🚀\n",
    ];
    for src in docs {
        let mut doc = Doc::open(SharedString::from(src));
        let check = |d: &Doc, s: &str| {
            // Doc 层 = `str::lines()` 口径（不含空文档的 max(1) 显示语义，那在
            // WyEditor::line_count 处补），编辑后仍与全文重算一致。
            assert_eq!(
                d.line_count(),
                s.lines().count(),
                "line_count 不一致 for {:?}",
                s
            );
            assert_eq!(d.char_count(), s.chars().count(), "char_count 不一致");
        };
        check(&doc, src);

        // 随机编辑序列：每轮与全文重算对拍。
        let mut s = src.to_string();
        let mut rng = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _ in 0..200 {
            let op = (next() % 3) as usize;
            // 只在字符边界取编辑点（String::drain 与 rope remove 都要求 UTF-8 对齐）。
            let mut pts: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
            pts.push(s.len());
            let n = pts.len();
            let a = pts[(next() % (n.max(1)) as u64) as usize];
            let b = pts[(next() % (n.max(1)) as u64) as usize];
            let (a, b) = (a.min(b), a.max(b));
            match op {
                0 => {
                    // 删除
                    if b > a {
                        doc.edit(a..b, "");
                        s.drain(a..b);
                    }
                }
                1 => {
                    // 插入
                    let ins = "中x\n🚀";
                    doc.edit(a..a, ins);
                    s.insert_str(a, ins);
                }
                _ => {
                    // 替换
                    let rep = if next() % 2 == 0 { "已✅" } else { "xy\nz" };
                    doc.edit(a..b, rep);
                    s.replace_range(a..b, rep);
                }
            }
            check(&doc, &s);
        }
    }
}

/// 全量换算参考（与 `wysiwyg::offset_to_utf16/offset_from_utf16` 同语义）。
fn ref_offset_to_utf16(s: &str, offset: usize) -> usize {
    s[..offset.min(s.len())].encode_utf16().count()
}

fn ref_offset_from_utf16(s: &str, utf16: usize) -> usize {
    let mut count = 0usize;
    for (i, c) in s.char_indices() {
        if count >= utf16 {
            return i;
        }
        count += c.len_utf16();
    }
    s.len()
}

/// 行粒度 UTF-16 前缀索引（`byte_to_utf16/utf16_to_byte/total_utf16/line_utf16_starts`）
/// 与全量重算逐值一致——IME 协议层换算的钉死护栏（改错会让候选窗/选区落点错位）。
/// Str 与 Rope 两源也必须一致。
#[test]
fn doc_utf16_index_matches_full_recompute() {
    use crate::markdown::doc::Doc;
    use gpui::SharedString;
    use ropey::Rope;

    let docs = [
        "",
        "a",
        "a\nb",
        "a\nb\n",
        "中文 🚀 emoji\n第二行\n",
        "a\n\nb\n\n",
    ];
    for src in docs {
        let mut doc = Doc::open(SharedString::from(src));
        let rope_doc = Doc::open_rope(Rope::from(src));

        let check = |d: &Doc, s: &str| {
            assert_eq!(
                d.line_utf16_starts().len(),
                d.line_starts().len(),
                "UTF-16 行索引与行表应同形"
            );
            assert_eq!(d.total_utf16(), s.encode_utf16().count(), "total_utf16 不一致");
            if d.line_starts().len() > 0 {
                assert_eq!(
                    d.line_utf16_starts()[0],
                    0,
                    "首行 UTF-16 前缀应为 0"
                );
            }
            // 字节 → UTF-16：对所有字符边界探测（含行首/文末）。
            let mut bytes: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
            bytes.push(s.len());
            for &b in &bytes {
                assert_eq!(
                    d.byte_to_utf16(b),
                    ref_offset_to_utf16(s, b),
                    "byte_to_utf16({b}) 不一致 for {:?}",
                    s
                );
            }
            // UTF-16 → 字节：行首/字符边界/越界（钳到文末）。
            let line_bases = d.line_utf16_starts().to_vec();
            let total = d.total_utf16();
            let mut u16s: Vec<usize> = vec![0, total, total + 3];
            u16s.extend(line_bases.iter().copied());
            for b in &bytes {
                u16s.push(d.byte_to_utf16(*b));
            }
            for &u in &u16s {
                assert_eq!(
                    d.utf16_to_byte(u),
                    ref_offset_from_utf16(s, u),
                    "utf16_to_byte({u}) 不一致 for {:?}",
                    s
                );
            }
        };
        check(&doc, src);
        check(&rope_doc, src);

        // 随机编辑序列后仍一致（前进式：doc 与 rope_doc 施加相同编辑，rgu66=i64）。
        let mut s = src.to_string();
        let mut doc2 = Doc::open(SharedString::from(src));
        let mut rng = 0x243F6A8885A308D3u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _ in 0..150 {
            let mut pts: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
            pts.push(s.len());
            let n = pts.len();
            let a = pts[(next() % (n.max(1)) as u64) as usize];
            let b = pts[(next() % (n.max(1)) as u64) as usize];
            let (a, b) = (a.min(b), a.max(b));
            let choices = ["中\n", "x", "你好🚀\n是", "\n\n"];
            let ins = choices[(next() % choices.len() as u64) as usize];
            match (next() % 3) as usize {
                0 => {
                    if b > a {
                        doc.edit(a..b, "");
                        doc2.edit(a..b, "");
                        s.drain(a..b);
                    }
                }
                1 => {
                    doc.edit(a..a, ins);
                    doc2.edit(a..a, ins);
                    s.insert_str(a, ins);
                }
                _ => {
                    doc.edit(a..b, ins);
                    doc2.edit(a..b, ins);
                    s.replace_range(a..b, ins);
                }
            }
            check(&doc, &s);
            check(&doc2, &s);
            // 双源索引逐项一致。
            assert_eq!(doc.line_utf16_starts(), doc2.line_utf16_starts());
        }
    }
}
