//! gpui-editor 的**叶子基础 crate**（仅依赖 gpui，无任何项目内依赖）。
//!
//! 承载各耦合环断点所需的最小共享件：
//! - [`EditIntent`]：编辑撤销分组的纯类型（editor↔ui/ime 环的下沉载体）；
//! - [`actions`]：应用级动作类型 + `bind_input_keys`（纯 gpui KeyBinding）；
//! - [`warm`]：冷启动 COLR emoji 光栅预热（**勿删**，见模块内注释）；
//! - [`util`]：UTF-8 字符边界钳制基元；
//! - [`diag`]：进程级 env 门控诊断开关 / 测试构建判定 / 诊断日志路径（各 crate 手写探针收敛地）；
//! - [`prof`]：性能剖析/操作级实时追踪微工具。

pub mod actions;
pub mod diag;
pub mod prof;
pub mod startup;
pub mod tab_diag;
pub mod util;
pub mod warm;

/// 编辑意图（决定撤销/重做合并规则，跨 editor/ui/ime/markdown 共享的纯类型）。
///
/// 自 `src/editor/undo.rs` 下沉到 base 以断开 editor↔ui、editor↔ime 两个耦合环；
/// `editor` 仍 re-export（`crate::editor::EditIntent`）保持对外兼容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditIntent {
    /// 连续打字（纯插入、无换行、邻接 → 合并成一个撤销单元）。
    Typing,
    /// Backspace 退格（纯删除、首尾相接 → 合并）。
    Backspace,
    /// 正向删除（纯删除、同起点 → 延伸合并）。
    Delete,
    /// 原子编辑（Enter/Tab/粘贴/程序化写 → 永不合并，并制造断层）。
    Atomic,
}
