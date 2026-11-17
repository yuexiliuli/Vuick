//! 缺陷 C12 复现红测：`math_to_segments`/`math_to_unicode` 对 `^` 组递归无深度上限。
//! 见 docs/code-review-2026-11-17/12-markdown_wysiwyg_math.md H（math.rs:496,510）。
//!
//! 期望语义（评审建议）：加递归深度上限（如 MAX_MATH_NEST=64）与输入长度上限，
//! 超限走兜底（Err/降级），不无限递归——即**超限额输入的展开应有界**。
//!
//! 红测断言：深度远超建议上限的 `^{…}` 嵌套输入，其展开字符数必须被截住；
//! 当前实现无 cap，深输入照算不降级 → 断言失败 = RED。
//! 本层避开能真打爆 debug 1MB 栈的输入（D=150 帧开销 ~40KB 安全），用「有界断言」姿势。

use markdown_wysiwyg_math::math_to_segments;

/// 演算（先注释演算再落值，K143/K177）：
/// 输入 = `"^{1".repeat(D) + "}".repeat(D)`，即 `^{1^{1^{…^{1}}}}`（D 层嵌套）。
/// 每层 `^` 后的 `read_group` 读整段平衡组 → 内层交给 `math_to_unicode` 递归，
/// 递归深度 ∝ D（正是「粘贴数千 ^ 在 debug 1MB 栈上 shape 期 crash」的机制）；
/// 每层 `1` 经 `sup_map` 成 `¹`、内层已上标字符 `¹` 透传，故**顶层 sup 段的字符数恰 = D**
/// （字节数 = 2·D，`¹` 为 U+00B9 双字节——断言必须以字符数计，不能以字节数计）。
/// D=150 → 150 字符 > 64（建议 MAX_MATH_NEST）→ 期望被兜底截住。
#[test]
fn deep_sup_nesting_expansion_is_bounded() {
    let depth = 150;
    let src = format!("{}{}", "^{1".repeat(depth), "}".repeat(depth));
    let segs = math_to_segments(&src);
    let chars: usize = segs.iter().map(|s| s.text.chars().count()).sum();
    assert!(
        chars <= 64,
        "深度 {depth} 的上标嵌套展开字符数 {chars} 应 ≤ 64（超限走兜底/降级）；\
         当前无递归深度上限 → 展开字符数 = 嵌套深度"
    );
}
