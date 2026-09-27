//! Mermaid 语法解析（纯函数，无窗口）：`.mermaid` 围栏源码 → flowchart [`MmGraph`] /
//! sequence [`MmSeq`] 语义模型。解析与布局、绘制解耦——`mermaid_card_height` 只用这里
//! 的纯结果（层数/行数），`mm_layout` 在 shape 期吃解析结果 + 文本测量。
//!
//! flowchart 语法覆盖：`graph/flowchart` 声明 + 方向、节点形状（`[]`/`{}`/`()`/`(())`/
//! `([ ])`/`[[ ]]`/`[( )]`/`{{ }}`/`> ]`/`[…]` 斜口变体/`((( )))`/`@{…}`）、边型与
//! 链式/并行、`%%` 注释、`subgraph … end`、`classDef/class/style`。sequence 覆盖：
//! `participant/actor … as …`、六种箭头（含 `+/-` 激活速记）、`activate/deactivate`、
//! `Note … of/over`、`loop/alt/else/opt/par … end`。

use super::{MmDirection, MmEdge, MmEdgeKind, MmGraph, MmNode, MmSeq, MmSeqArrow, MmSeqFragment, MmSeqFragmentKind, MmSeqMessage, MmSeqNote, MmSeqNotePos, MmSeqParticipant, MmSeqRow, MmShape, MmStyle, MmSubgraph};
use std::collections::HashMap;

/// 图类型判定（首个非注释/空行）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MmDiagramKind {
    Flow,
    Seq,
    Empty,
}

/// 去掉 `%%` 注释（不含引号内的 `%%`）。
fn strip_comment(line: &str) -> &str {
    let mut in_q: Option<char> = None;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let c = bytes[i] as char;
        if let Some(q) = in_q {
            if c == q {
                in_q = None;
            }
        } else if c == '"' || c == '\'' {
            in_q = Some(c);
        } else if c == '%' && bytes[i + 1] as char == '%' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

pub(crate) fn diagram_kind(source: &str) -> MmDiagramKind {
    for raw in source.lines() {
        let t = strip_comment(raw).trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("sequenceDiagram") {
            return MmDiagramKind::Seq;
        }
        return MmDiagramKind::Flow;
    }
    MmDiagramKind::Empty
}

/// 形状关键词（`@{}` 语法）。
fn shape_from_keyword(kw: &str) -> MmShape {
    match kw.to_ascii_lowercase().as_str() {
        "diamond" | "rhombus" => MmShape::Diamond,
        "round" | "rounded" => MmShape::Round,
        "stadium" => MmShape::Stadium,
        "circle" => MmShape::Circle,
        "doublecircle" => MmShape::DoubleCircle,
        "subroutine" => MmShape::Subroutine,
        "cylinder" | "database" => MmShape::Cylinder,
        "hexagon" => MmShape::Hexagon,
        "asymmetric" => MmShape::Asymmetric,
        "parallelogram" => MmShape::Parallelogram,
        "trapezoid" => MmShape::Trapezoid,
        _ => MmShape::Rect,
    }
}

/// 解析节点引用 `A[…]` / `A@{…}` / 裸 `A` → (id, 可选标签, 形状)。
fn mm_node(s: &str) -> (String, Option<String>, MmShape) {
    let s = s.trim();
    if s.is_empty() {
        return (String::new(), None, MmShape::Rect);
    }
    // `@{}` 通用形状语法：`A@{ shape: rect, label: "x" }`（可带尾随 `-->…`）。
    if let Some(oi) = s.find("@{") {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 2..];
        let mut shp = MmShape::Rect;
        let mut lab: Option<String> = None;
        if let Some(inner) = find_braced(rest) {
            for part in inner.split(',') {
                let p = part.trim();
                if let Some(v) = p.strip_prefix("shape:") {
                    shp = shape_from_keyword(v.trim());
                } else if let Some(v) = p.strip_prefix("label:") {
                    lab = Some(clean_quoted(v.trim()).replace("\\\"", "\""));
                }
            }
        }
        return (id, lab, shp);
    }
    // 双圆 `((( … )))` / 圆 `(( … ))`。
    if let Some(oi) = s.find("((") {
        let id = s[..oi].trim().to_string();
        let depth = if s.as_bytes().get(oi + 2) == Some(&b'(') { 3 } else { 2 };
        let close: String = ")".repeat(depth);
        let rest = &s[oi + depth..];
        let shp = if depth == 3 { MmShape::DoubleCircle } else { MmShape::Circle };
        return match rest.find(&close) {
            Some(ci) => (id, Some(rest[..ci].trim().to_string()), shp),
            None => (id, None, shp),
        };
    }
    // 圆柱 `[( … )]` / 跑道 `([ … ])` / 子程序 `[[ … ]]` / 六边形 `{{ … }}`。
    for (open, close, shp) in [
        ("[(", ")]", MmShape::Cylinder),
        ("([", "])", MmShape::Stadium),
        ("[[", "]]", MmShape::Subroutine),
        ("{{", "}}", MmShape::Hexagon),
    ] {
        if let Some(oi) = s.find(open) {
            let id = s[..oi].trim().to_string();
            let rest = &s[oi + open.len()..];
            return match rest.find(close) {
                Some(ci) => (id, Some(rest[..ci].trim().to_string()), shp),
                None => (id, None, shp),
            };
        }
    }
    // 不对称旗形 `> … ]`。
    if let Some(oi) = s.find('>') {
        if oi > 0 {
            let id = s[..oi].trim().to_string();
            let rest = &s[oi + 1..];
            return match rest.find(']') {
                Some(ci) => (id, Some(rest[..ci].trim().to_string()), MmShape::Asymmetric),
                None => (id, None, MmShape::Asymmetric),
            };
        }
    }
    // 菱形 `{ … }`。
    if let Some(oi) = s.find('{') {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 1..];
        return match rest.find('}') {
            Some(ci) => (id, Some(rest[..ci].trim().to_string()), MmShape::Diamond),
            None => (id, None, MmShape::Diamond),
        };
    }
    // 圆角 `( … )`（注意 `((` 已在上方处理）。
    if let Some(oi) = s.find('(') {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 1..];
        return match rest.find(')') {
            Some(ci) => (id, Some(rest[..ci].trim().to_string()), MmShape::Round),
            None => (id, None, MmShape::Round),
        };
    }
    // 方括号 `[ … ]`（含斜口变体）。
    if let Some(oi) = s.find('[') {
        let id = s[..oi].trim().to_string();
        let rest = &s[oi + 1..];
        return match rest.find(']') {
            Some(ci) => {
                let (lab, shp) = bracket_labelled(&rest[..ci]);
                (id, lab.map(str::to_string), shp)
            }
            None => (id, None, MmShape::Rect),
        };
    }
    (s.to_string(), None, MmShape::Rect)
}

/// `[ … ]` 内文 → (可选标签, 形状)：按首尾斜杠形态区分矩形/平行四边形/梯形。
/// mermaid 映射：`[/…/]` 平行四边形；`[\…\]` 平行四边形（反）；`[/…\]` 梯形；
/// `[\…/]` 梯形（反）。
fn bracket_labelled(inner: &str) -> (Option<&str>, MmShape) {
    let s = inner.trim();
    let starts_slash = s.starts_with('/');
    let starts_back = s.starts_with('\\');
    let ends_slash = s.ends_with('/');
    let ends_back = s.ends_with('\\');
    let shp = match (starts_slash, starts_back, ends_slash, ends_back) {
        (true, _, true, _) => MmShape::Parallelogram,
        (_, true, _, true) => MmShape::ParallelogramAlt,
        (true, _, _, true) => MmShape::Trapezoid,
        (_, true, true, _) => MmShape::TrapezoidAlt,
        _ => MmShape::Rect,
    };
    if starts_slash || starts_back {
        if s.len() >= 2 {
            return (Some(s[1..s.len() - 1].trim()), shp);
        }
        return (Some(""), shp);
    }
    (Some(s), shp)
}

/// 去首尾引号。
fn clean_quoted(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 找第一个未闭合于引号内的 `}`，返回其前内容。
fn find_braced(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut in_q: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        if let Some(q) = in_q {
            if c == q {
                in_q = None;
            }
        } else if c == b'"' || c == b'\'' {
            in_q = Some(c);
        } else if c == b'}' {
            return Some(&s[..i]);
        }
    }
    None
}

/// 边型规格：(结束标记, 起始标记, 边型)。
const EDGE_SPECS: &[(&str, &str, MmEdgeKind)] = &[
    ("<==>", "<=", MmEdgeKind::BothThick),
    ("<-->", "<-", MmEdgeKind::BothArrow),
    ("==>", "==", MmEdgeKind::ThickArrow),
    ("===", "==", MmEdgeKind::ThickLine),
    ("-.->", "-.", MmEdgeKind::DottedArrow),
    ("-.-", "-.", MmEdgeKind::DottedLine),
    ("..>", "..", MmEdgeKind::DottedArrow),
    ("...", "..", MmEdgeKind::DottedLine),
    ("-->", "--", MmEdgeKind::Arrow),
    ("---", "--", MmEdgeKind::Line),
    ("--o", "--", MmEdgeKind::CircleEnd),
    ("--x", "--", MmEdgeKind::CrossEnd),
];

/// 从文本中找最早出现的边结束标记。
fn earliest_op(s: &str) -> Option<(usize, &'static str, &'static str, MmEdgeKind)> {
    let mut best: Option<(usize, &'static str, &'static str, MmEdgeKind)> = None;
    for (end, start, kind) in EDGE_SPECS {
        if let Some(i) = s.find(end) {
            if best.map_or(true, |(bi, _, _, _)| i < bi) {
                best = Some((i, end, start, *kind));
            }
        }
    }
    best
}

/// `rhs` 中 to 节点结束的位置（下一个边结束标记），无则到串尾。
fn split_to_node(rhs: &str) -> (&str, &str) {
    let mut m: Option<usize> = None;
    for (e2, _, _) in EDGE_SPECS {
        if let Some(j) = rhs.find(e2) {
            m = Some(m.map_or(j, |mm: usize| mm.min(j)));
        }
    }
    match m {
        Some(j) => (&rhs[..j], &rhs[j..]),
        None => (rhs, ""),
    }
}

/// 按括号外的 `&` 拆分：`A --> B & C --> D` → 两条链路。
fn split_amp(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut in_q: Option<char> = None;
    for (i, c) in s.char_indices() {
        if let Some(q) = in_q {
            if c == q {
                in_q = None;
            }
        } else if c == '"' || c == '\'' {
            in_q = Some(c);
        } else if c == '[' {
            depth += 1;
        } else if c == ']' {
            depth -= 1;
        } else if c == '&' && depth == 0 {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

/// 注册节点（去重；已有则仅当本次带**显式标签**才覆盖——裸引用不得覆盖 `B{判断}`），
/// 并把节点登记进当前所有打开的 subgraph。
fn register_node(
    nodes: &mut Vec<MmNode>,
    id: &str,
    lab: Option<String>,
    shp: MmShape,
    open_sub: &[usize],
    subgraphs: &mut [MmSubgraph],
) {
    if id.is_empty() {
        return;
    }
    let explicit = lab.as_ref().map(|l| !l.trim().is_empty()).unwrap_or(false);
    if let Some(n) = nodes.iter_mut().find(|n| n.id == id) {
        if explicit {
            n.label = lab.unwrap();
            n.shape = shp;
        }
    } else {
        let label = if explicit { lab.unwrap() } else { id.to_string() };
        nodes.push(MmNode { id: id.to_string(), label, shape: shp });
    }
    for &si in open_sub {
        if let Some(sg) = subgraphs.get_mut(si) {
            if !sg.nodes.iter().any(|x| x == id) {
                sg.nodes.push(id.to_string());
            }
        }
    }
}

/// 解析单个链路（可能链式），把端点节点与边都产出。
/// 返回该链**第一条边的起点 id**（用于 `&` 裸节点段续接同一 source：`X --> Y & Z` = X→Y、X→Z）。
fn parse_single_chain(
    chunk: &str,
    edges: &mut Vec<MmEdge>,
    nodes: &mut Vec<MmNode>,
    open_sub: &[usize],
    subgraphs: &mut [MmSubgraph],
) -> Option<String> {
    let mut s: &str = chunk.trim();
    if s.is_empty() {
        return None;
    }
    let mut first_from: Option<String> = None;
    let mut prev: Option<(String, Option<String>, MmShape)> = None;
    loop {
        let (i, end, start, kind) = match earliest_op(s) {
            Some(x) => x,
            None => break,
        };
        if i == 0 {
            // 链式续接：上一 to 节点作为本段 from。
            let p = match prev.take() {
                Some(p) => p,
                None => break,
            };
            let mut rhs_raw = &s[end.len()..];
            let mut pipe = None;
            if let Some(r) = rhs_raw.strip_prefix('|') {
                if let Some(cj) = r.find('|') {
                    pipe = Some(r[..cj].trim().to_string());
                    rhs_raw = &r[cj + 1..];
                }
            }
            let (to_part, rest) = split_to_node(rhs_raw);
            let b = mm_node(to_part);
            if b.0.is_empty() {
                break;
            }
            let (bid, blab, bshp) = b;
            register_node(nodes, &p.0, p.1, p.2, open_sub, subgraphs);
            register_node(nodes, &bid, blab, bshp, open_sub, subgraphs);
            edges.push(MmEdge { from: p.0.clone(), to: bid.clone(), kind, label: pipe });
            prev = Some((bid, None, bshp));
            s = rest;
            continue;
        }
        let lhs_all = &s[..i];
        let mut rhs_raw = &s[i + end.len()..];
        let (lhs, label_at) = match lhs_all.strip_suffix(start) {
            Some(x) => (x.trim(), None),
            None => match lhs_all.rfind(start) {
                Some(k) => (lhs_all[..k].trim(), Some(lhs_all[k + start.len()..].trim().to_string())),
                None => (lhs_all.trim(), None),
            },
        };
        if lhs.is_empty() {
            break;
        }
        let mut pipe = None;
        if let Some(r) = rhs_raw.strip_prefix('|') {
            if let Some(cj) = r.find('|') {
                pipe = Some(r[..cj].trim().to_string());
                rhs_raw = &r[cj + 1..];
            }
        }
        let (to_part, rest) = split_to_node(rhs_raw);
        let a = mm_node(lhs);
        let b = mm_node(to_part);
        if a.0.is_empty() || b.0.is_empty() {
            break;
        }
        let (aid, alab, ashp) = a;
        let (bid, blab, bshp) = b;
        register_node(nodes, &aid, alab, ashp, open_sub, subgraphs);
        register_node(nodes, &bid, blab, bshp, open_sub, subgraphs);
        let label = label_at.or(pipe);
        edges.push(MmEdge { from: aid.clone(), to: bid.clone(), kind, label });
        if first_from.is_none() {
            first_from = Some(aid.clone());
        }
        prev = Some((bid, None, bshp));
        s = rest;
    }
    first_from
}

/// 解析 `fill:#hex,stroke:#hex,stroke-width:Npx` → MmStyle。
fn parse_decl_style(rest: &str) -> MmStyle {
    let mut st = MmStyle::default();
    for part in rest.split(',') {
        let p = part.trim();
        if let Some(v) = p.strip_prefix("fill:") {
            st.fill = parse_hex_color(v);
        } else if let Some(v) = p.strip_prefix("stroke-width:") {
            let num: String = v.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
            st.stroke_width = num.parse::<f32>().ok();
        } else if let Some(v) = p.strip_prefix("stroke:") {
            st.stroke = parse_hex_color(v);
        }
    }
    st
}

/// `fill` / `stroke` 值（`#9f6`、`#99ff66`）→ 0xRRGGBB。
fn parse_hex_color(v: &str) -> Option<u32> {
    let v = v.trim().trim_start_matches('#');
    let digits: String = v.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    match digits.len() {
        3 => {
            let r = u32::from_str_radix(&digits[0..1], 16).ok()?;
            let g = u32::from_str_radix(&digits[1..2], 16).ok()?;
            let b = u32::from_str_radix(&digits[2..3], 16).ok()?;
            Some((r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b)
        }
        6 => u32::from_str_radix(&digits, 16).ok(),
        _ => None,
    }
}

/// 解析 `graph`/`flowchart` 头部的方向。
fn parse_direction(t: &str) -> MmDirection {
    let lc = t.to_ascii_lowercase();
    if lc.contains("lr") {
        MmDirection::LR
    } else if lc.contains("rl") {
        MmDirection::RL
    } else if lc.contains("bt") {
        MmDirection::BT
    } else {
        MmDirection::TB
    }
}

/// 解析 flowchart 源码 → [`MmGraph`]。
pub(crate) fn mermaid_graph(source: &str) -> MmGraph {
    let mut nodes: Vec<MmNode> = Vec::new();
    let mut edges: Vec<MmEdge> = Vec::new();
    let mut subgraphs: Vec<MmSubgraph> = Vec::new();
    let mut class_defs: HashMap<String, MmStyle> = HashMap::new();
    let mut class_assign: Vec<(Vec<String>, String)> = Vec::new();
    let mut node_style: HashMap<String, MmStyle> = HashMap::new();
    let mut direction = MmDirection::TB;
    let mut open_sub: Vec<usize> = Vec::new();

    for raw in source.lines() {
        let t = strip_comment(raw).trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("graph ") || t.starts_with("flowchart ") {
            let rest = t
                .strip_prefix("graph ")
                .or_else(|| t.strip_prefix("flowchart "))
                .unwrap_or("");
            direction = parse_direction(rest);
            continue;
        }
        if t.starts_with("sequenceDiagram") {
            continue;
        }
        if let Some(rest) = t.strip_prefix("subgraph ") {
            parse_subgraph_open(rest.trim(), &mut subgraphs, &mut open_sub);
            continue;
        }
        if t == "end" && !open_sub.is_empty() {
            open_sub.pop();
            continue;
        }
        if let Some(rest) = t.strip_prefix("classDef ") {
            let mut it = rest.splitn(2, char::is_whitespace);
            let name = it.next().unwrap_or("").trim().to_string();
            let body = it.next().unwrap_or("").trim().to_string();
            if !name.is_empty() {
                class_defs.insert(name, parse_decl_style(&body));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("class ") {
            let (ids, cls) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                None => (String::new(), String::new()),
            };
            let ids: Vec<String> = ids.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
            if !ids.is_empty() && !cls.is_empty() {
                class_assign.push((ids, cls));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("style ") {
            let (id, body) = match rest.split_once(char::is_whitespace) {
                Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
                None => (rest.trim().to_string(), String::new()),
            };
            if !id.is_empty() {
                node_style.insert(id, parse_decl_style(&body));
            }
            continue;
        }
        if t.starts_with("title ") {
            continue;
        }
        // 边（链式/并行）。`&` 后的**裸节点段**（无边操作符）续接前一条边的起点——
        // `X --> Y & Z` 语义 = X→Y 与 X→Z；整段均为完整边（`A --> B & C --> D`）则各成边。
        if earliest_op(t).is_some() {
            let mut pending_source: Option<String> = None;
            for chunk in split_amp(t) {
                match parse_single_chain(chunk, &mut edges, &mut nodes, &open_sub, &mut subgraphs) {
                    Some(from) => pending_source = Some(from),
                    None => {
                        // 裸节点段：接 pending source。
                        let (id, lab, shp) = mm_node(chunk);
                        if id.is_empty() {
                            continue;
                        }
                        if let Some(src) = &pending_source {
                            register_node(&mut nodes, src, None, MmShape::Rect, &open_sub, &mut subgraphs);
                            register_node(&mut nodes, &id, lab, shp, &open_sub, &mut subgraphs);
                            edges.push(MmEdge { from: src.clone(), to: id.clone(), kind: MmEdgeKind::Arrow, label: None });
                        } else {
                            register_node(&mut nodes, &id, lab, shp, &open_sub, &mut subgraphs);
                        }
                    }
                }
            }
            continue;
        }
        // 独立节点定义。
        let (id, lab, shp) = mm_node(t);
        if !id.is_empty() {
            register_node(&mut nodes, &id, lab, shp, &open_sub, &mut subgraphs);
        }
    }
    // 解析每节点最终样式：`class`（后到优先）→ `style <id>` 覆盖。
    let mut styles: HashMap<String, MmStyle> = HashMap::new();
    for n in &nodes {
        let mut st = MmStyle::default();
        for (ids, cls) in &class_assign {
            if ids.iter().any(|x| x == &n.id) {
                if let Some(c) = class_defs.get(cls) {
                    st = *c;
                }
            }
        }
        if let Some(ov) = node_style.get(&n.id) {
            st.merge(*ov);
        }
        if st.fill.is_some() || st.stroke.is_some() || st.stroke_width.is_some() {
            styles.insert(n.id.clone(), st);
        }
    }
    MmGraph { direction, nodes, edges, subgraphs, styles }
}

/// 解析 `subgraph id[Title]` / `subgraph id Title` / `subgraph Title` 开行。
fn parse_subgraph_open(rest: &str, subgraphs: &mut Vec<MmSubgraph>, open_sub: &mut Vec<usize>) {
    let rest = rest.trim();
    let (id, title) = if let Some(oi) = rest.find('[') {
        let id = rest[..oi].trim().to_string();
        let title = rest[oi + 1..].trim_end_matches(']').trim().to_string();
        (if id.is_empty() { title.clone() } else { id }, title)
    } else {
        let mut it = rest.splitn(2, char::is_whitespace);
        let first = it.next().unwrap_or("").trim().to_string();
        let second = it.next().unwrap_or("").trim().to_string();
        if second.is_empty() {
            (first.clone(), first)
        } else {
            (first, second)
        }
    };
    if id.is_empty() {
        return;
    }
    let depth = open_sub.len();
    open_sub.push(subgraphs.len());
    subgraphs.push(MmSubgraph { id, title, depth, nodes: Vec::new() });
}

// ---------------------------------------------------------------------------
// 序列图解析
// ---------------------------------------------------------------------------

/// 消息箭头（长先）：返回 (匹配处索引, 箭头, 该箭头字符长度)。
fn parse_seq_arrow(t: &str) -> Option<(usize, MmSeqArrow, usize)> {
    for (pat, arrow) in [
        ("-->>", MmSeqArrow::Dotted),
        ("-->", MmSeqArrow::DottedOpen),
        ("->>", MmSeqArrow::Solid),
        ("--x", MmSeqArrow::DottedCross),
        ("--)", MmSeqArrow::DottedOpen),
        ("-x", MmSeqArrow::SolidCross),
        ("->", MmSeqArrow::SolidOpen),
        ("-)", MmSeqArrow::SolidOpen),
    ] {
        if let Some(i) = t.find(pat) {
            return Some((i, arrow, pat.len()));
        }
    }
    None
}

/// 解析 `participant/actor` 行 → (id, label, is_actor)。
fn parse_seq_participant(body: &str, is_actor: bool) -> Option<(String, String, bool)> {
    let body = body.trim();
    if let Some((a, b)) = body.split_once(char::is_whitespace) {
        let b = b.trim();
        if b.starts_with("as") {
            let label = b.trim_start_matches("as").trim().to_string();
            let id = a.trim().to_string();
            return Some((id.clone(), if label.is_empty() { id.clone() } else { label }, is_actor));
        }
    }
    let id = body.split_whitespace().next().unwrap_or("").to_string();
    Some((id.clone(), id, is_actor))
}

/// 参与者（隐式声明按首见顺序补入）。
fn ensure_participant(participants: &mut Vec<MmSeqParticipant>, id: &str, label: Option<String>, is_actor: bool) {
    if id.is_empty() {
        return;
    }
    if !participants.iter().any(|p| p.id == id) {
        let label = label.unwrap_or_else(|| id.to_string());
        participants.push(MmSeqParticipant { id: id.to_string(), label, is_actor });
    }
}

/// 解析序列图源码 → [`MmSeq`]。
pub(crate) fn mermaid_seq(source: &str) -> MmSeq {
    let mut participants: Vec<MmSeqParticipant> = Vec::new();
    let mut messages: Vec<MmSeqMessage> = Vec::new();
    let mut notes: Vec<MmSeqNote> = Vec::new();
    let mut fragments: Vec<MmSeqFragment> = Vec::new();
    let mut rows: Vec<MmSeqRow> = Vec::new();
    let mut frag_stack: Vec<usize> = Vec::new();

    for raw in source.lines() {
        let t = strip_comment(raw).trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with("sequenceDiagram") {
            continue;
        }
        if let Some(rest) = t.strip_prefix("participant ") {
            if let Some((id, label, _)) = parse_seq_participant(rest, false) {
                ensure_participant(&mut participants, &id, Some(label), false);
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("actor ") {
            if let Some((id, label, _)) = parse_seq_participant(rest, true) {
                ensure_participant(&mut participants, &id, Some(label), true);
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("activate ") {
            let id = rest.trim().to_string();
            if !id.is_empty() {
                ensure_participant(&mut participants, &id, None, false);
                rows.push(MmSeqRow::Activate(id));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("deactivate ") {
            let id = rest.trim().to_string();
            if !id.is_empty() {
                rows.push(MmSeqRow::Deactivate(id));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("Note ") {
            parse_seq_note(rest.trim(), &mut participants, &mut notes, &mut rows);
            continue;
        }
        // 片段。
        if let Some(rest) = t.strip_prefix("loop ") {
            open_fragment(&mut fragments, &mut frag_stack, &mut rows, MmSeqFragmentKind::Loop, rest.trim());
            continue;
        }
        if let Some(rest) = t.strip_prefix("alt ") {
            open_fragment(&mut fragments, &mut frag_stack, &mut rows, MmSeqFragmentKind::Alt, rest.trim());
            continue;
        }
        if let Some(rest) = t.strip_prefix("opt ") {
            open_fragment(&mut fragments, &mut frag_stack, &mut rows, MmSeqFragmentKind::Opt, rest.trim());
            continue;
        }
        if let Some(rest) = t.strip_prefix("par ") {
            open_fragment(&mut fragments, &mut frag_stack, &mut rows, MmSeqFragmentKind::Par, rest.trim());
            continue;
        }
        if t.starts_with("else") {
            if let Some(&fi) = frag_stack.last() {
                if let Some(f) = fragments.get_mut(fi) {
                    let title = t[4..].trim().to_string();
                    let title = if title.is_empty() { None } else { Some(title) };
                    f.else_titles.push(title.clone());
                    rows.push(MmSeqRow::FragElse(title));
                }
            }
            continue;
        }
        if t == "end" {
            if let Some(fi) = frag_stack.pop() {
                rows.push(MmSeqRow::FragClose(fi));
            }
            continue;
        }
        // 消息。
        if let Some((i, arrow, marker_len)) = parse_seq_arrow(t) {
            let lhs = t[..i].trim();
            let rhs = t[i + marker_len..].trim();
            // 激活速记 `+B` / `-B` 在 to 上。
            let from = lhs.trim_end_matches('(').trim_end_matches(')').trim().to_string();
            let (to, act) = if let Some(r) = rhs.strip_prefix('+') {
                (r.trim().to_string(), 1i8)
            } else if let Some(r) = rhs.strip_prefix('-') {
                (r.trim().to_string(), -1i8)
            } else {
                (rhs.to_string(), 0i8)
            };
            let (to, text) = match to.split_once(':') {
                Some((tp, tx)) => (tp.trim().to_string(), tx.trim().to_string()),
                None => (to.clone(), String::new()),
            };
            if from.is_empty() || to.is_empty() {
                continue;
            }
            ensure_participant(&mut participants, &from, None, false);
            ensure_participant(&mut participants, &to, None, false);
            messages.push(MmSeqMessage { from, to, arrow, text, act });
            rows.push(MmSeqRow::Message(messages.len() - 1));
            continue;
        }
        // 其它（autonumber 等）忽略。
    }
    MmSeq { participants, messages, notes, fragments, rows }
}

/// 片段开行。
fn open_fragment(
    fragments: &mut Vec<MmSeqFragment>,
    frag_stack: &mut Vec<usize>,
    rows: &mut Vec<MmSeqRow>,
    kind: MmSeqFragmentKind,
    title: &str,
) {
    fragments.push(MmSeqFragment { kind, title: title.to_string(), else_titles: Vec::new() });
    let fi = fragments.len() - 1;
    frag_stack.push(fi);
    rows.push(MmSeqRow::FragOpen(fi));
}

/// `Note [right of|left of|over] A[,B]: text`。
fn parse_seq_note(
    rest: &str,
    participants: &mut Vec<MmSeqParticipant>,
    notes: &mut Vec<MmSeqNote>,
    rows: &mut Vec<MmSeqRow>,
) {
    let (pos, a, b, raw_text) = if let Some(r) = rest.strip_prefix("right of ") {
        (MmSeqNotePos::Right, r.trim().to_string(), None, String::new())
    } else if let Some(r) = rest.strip_prefix("left of ") {
        (MmSeqNotePos::Left, r.trim().to_string(), None, String::new())
    } else if let Some(r) = rest.strip_prefix("over ") {
        let (ab, tx) = match r.split_once(':') {
            Some((ab, tx)) => (ab, tx),
            None => (r, ""),
        };
        if let Some(c) = ab.find(',') {
            (MmSeqNotePos::Over, ab[..c].trim().to_string(), Some(ab[c + 1..].trim().to_string()), tx.to_string())
        } else {
            (MmSeqNotePos::Over, ab.trim().to_string(), None, tx.to_string())
        }
    } else {
        return;
    };
    let (a, text) = if raw_text.is_empty() {
        match a.split_once(':') {
            Some((aa, tx)) => (aa.trim().to_string(), tx.trim().to_string()),
            None => (a, String::new()),
        }
    } else {
        (a, raw_text.trim().to_string())
    };
    if a.is_empty() {
        return;
    }
    ensure_participant(participants, &a, None, false);
    if let Some(bb) = &b {
        ensure_participant(participants, bb, None, false);
    }
    notes.push(MmSeqNote { pos, a, b, text });
    rows.push(MmSeqRow::Note(notes.len() - 1));
}
