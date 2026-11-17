//! 缺陷 C13 复现红测（B1）：mermaid 解析器不感知括号/冒号围栏。
//! 见 docs/code-review-2026-11-17/11-markdown_wysiwyg_mermaid.md H（parse.rs:242,254,268,601）。
//!
//! 期望语义：`[]`/`{}` 括号围栏与 `:` 消息文本内出现的箭头符号**不算**分隔 op。
//! 当前 `earliest_op`/`split_to_node`/`split_amp`/`parse_seq_arrow` 对整行裸扫
//! `-->`/`==>`/`-.-` 与 `&` → 括号/冒号里任何「长得像箭头」的合法 label/text 即静默错图
//! （拆成坏节点 + 坏边 / 消息 from/to/箭头全错）→ 断言不满足 = RED。

use markdown_wysiwyg_mermaid::{MmEdgeKind, MmSeqArrow, mermaid_graph, mermaid_seq};

/// `A[开始 --> 处理] --> B`：方括号内的 `-->` 是标签文本，不是分隔 op。
/// 期望：A 标签完整保留「开始 --> 处理」、目标节点 B 被解析出、唯一一条边 A→B。
#[test]
fn arrow_inside_square_bracket_label_is_not_an_edge() {
    let g = mermaid_graph("flowchart TD\nA[开始 --> 处理] --> B");
    let a = g.nodes.iter().find(|n| n.id == "A").expect("应有节点 A");
    assert_eq!(a.label, "开始 --> 处理", "A 的方括号标签应完整保留箭头文本");
    assert!(g.nodes.iter().any(|n| n.id == "B"), "目标节点 B 应被解析出来");
    assert_eq!(g.edges.len(), 1, "应恰有一条边");
    assert_eq!(
        (g.edges[0].from.as_str(), g.edges[0].to.as_str()),
        ("A", "B"),
        "唯一边应为 A→B"
    );
    assert_eq!(g.edges[0].kind, MmEdgeKind::Arrow, "边型应为实线箭头");
}

/// `A{是 --> 否} --> B`：花括号（菱形）内的 `-->` 同理不算 op。
#[test]
fn arrow_inside_curly_bracket_label_is_not_an_edge() {
    let g = mermaid_graph("flowchart TD\nA{是 --> 否} --> B");
    let a = g.nodes.iter().find(|n| n.id == "A").expect("应有节点 A");
    assert_eq!(a.label, "是 --> 否", "A 的花括号标签应完整保留箭头文本");
    assert!(g.nodes.iter().any(|n| n.id == "B"), "目标节点 B 应被解析出来");
    assert_eq!(g.edges.len(), 1, "应恰有一条边");
    assert_eq!(
        (g.edges[0].from.as_str(), g.edges[0].to.as_str()),
        ("A", "B"),
        "唯一边应为 A→B"
    );
}

/// `A->>B: install --> ok`：`:` 冒号后消息文本里的 `-->` 是正文，不是消息箭头。
/// 期望：真实箭头为 `->>`（Solid）、from=A、to=B、text 完整保留「install --> ok」。
#[test]
fn seq_message_text_arrow_is_not_the_message_arrow() {
    let seq = mermaid_seq("sequenceDiagram\nA->>B: install --> ok");
    assert_eq!(seq.messages.len(), 1, "应恰有一条消息");
    let m = &seq.messages[0];
    assert_eq!(m.from, "A", "消息方应为 A");
    assert_eq!(m.to, "B", "消息对象应为 B");
    assert_eq!(m.arrow, MmSeqArrow::Solid, "真实箭头应为 `->>`（Solid），非消息文本中的 `-->`");
    assert_eq!(m.text, "install --> ok", "冒号后消息文本应完整保留");
}
