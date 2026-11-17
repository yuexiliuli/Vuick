//! 缺陷 C13 复现红测（B2）：长边插桩虚节点 O(n·e) 无总量护栏。
//! 见 docs/code-review-2026-11-17/11-markdown_wysiwyg_mermaid.md H（layout.rs:326-339 + 208-227）。
//!
//! 期望语义（评审建议）：虚节点总数有硬上界（护栏，数量级 1e4）+ 最长路径记录防死循环；
//! 超限走兜底（跳过/截断部分长边插桩），不在 shape 期 UI 线程 OOM/挂死。
//! 当前 `flow_elements` 对 layer 跨 ≥2 层的每条边逐层插虚节点、无总量上限 →
//! 构造的图上虚节点数远超护栏 → 断言失败 = RED。
//! 输入维持可快速算完的量级（~12.7k 元素），不会真的 OOM/挂死。

use gpui::{Font, Hsla, TestAppContext, px};
use markdown_wysiwyg_mermaid::{MmLayout, MmTheme, mm_layout};

/// 测试用主题（默认字体/纯色；虚节点固定小宽，不依赖真实文本测量结果）。
fn test_theme() -> MmTheme {
    MmTheme {
        text: Hsla::default(),
        muted: Hsla::default(),
        mono: Font::default(),
        font: Font::default(),
        font_size: px(14.),
        code_bg: Hsla::default(),
        code_inline_bg: Hsla::default(),
        table_head_bg: Hsla::default(),
        accent: Hsla::default(),
        mermaid_accents: [Hsla::default(); 6],
        mermaid_border: Hsla::default(),
    }
}

/// 一条主链把层推高 + 若干同源长边：`A0 --> Ak`（k≥2）每边跨 k 层、各插 k-1 个虚节点。
fn long_edge_graph(n_nodes: usize) -> String {
    // `n_nodes` 个节点 A0..A_{n-1}；主链 A_i→A_{i+1} 经 layer_map 使 layer[A_i] = i（0..n-1）。
    let mut s = String::from("flowchart TD\n");
    for i in 0..n_nodes.saturating_sub(1) {
        s.push_str(&format!("A{i} --> A{}\n", i + 1));
    }
    // 长边 A0 → A_k（k=2..n-1）：layer 跨 0..k → 各插 k-1 个虚节点。
    for k in 2..n_nodes {
        s.push_str(&format!("A0 --> A{k}\n"));
    }
    s
}

/// 演算（先注释演算再落值，K143）：n=160 → 层 A_i 落在 i（0..159）。
/// 长边 A0→A_k（k=2..159）各插 (k-1) 个虚节点，
/// 总数 = Σ_{k=2}^{159}(k-1) = 158×159/2 = 12561 > 护栏 10000。
/// 当前无护栏 → mm_layout 产出 12561 个虚节点 → 「≤ 10000」断言失败 = RED。
#[gpui::test]
fn layout_virtual_node_count_is_capped(cx: &mut TestAppContext) {
    let vcx = cx.add_empty_window();
    let n_virtual = vcx.update(|window, _cx| {
        let src = long_edge_graph(160);
        match mm_layout(window, &test_theme(), &src, 0., 0., 800.) {
            MmLayout::Flow(f) => f.placed.iter().filter(|p| p.virtual_node).count(),
            _ => panic!("应为 Flow 布局"),
        }
    });
    assert!(
        n_virtual <= 10_000,
        "长边插桩虚节点总数 {n_virtual} 应 ≤ 护栏 10000（当前无护栏 → 超限）"
    );
}
