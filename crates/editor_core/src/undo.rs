//! 撤销/重做：确定性事务合并 + 300ms 时间窗。
//!
//! 设计吸收 **gpui-component `UndoManager`**：
//! - 事务 = 「一组编辑 + 编辑前后选区」，编辑与光标绑定恢复（zed 也是「事务即编辑单元」）。
//! - **确定性合并 + 时间窗**：按 `EditIntent` + 邻接性确定性判定（连续打字合并；Backspace
//!   首尾相接合并；Delete 同点延伸合并；Atomic/换行永远独立）；**I1 时间窗**：与上一编辑
//!   停顿 ≥[`UNDO_GROUP_MS`]（300ms）即新起撤销单元（对齐 zed `History.group_interval`，
//!   std 单调钟纯时间判定、无定时器副作用）。光标移动即 `set_cooperative(false)` 断层。
//! - 上限 [`MAX_UNDO_TXS`] 个事务 + [`UNDO_BYTE_CAP`] 字节（`Edit` 的 old+new 全量
//!   拷贝累计），双双超出丢最早；单条超限大事务至少保留一条（防空栈失守）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::editor::buffer::Edit;

pub const MAX_UNDO_TXS: usize = 1000;

/// 撤销栈字节上限：栈内每条 `Edit` 的 `old_text.len() + new_text.len()` 全量拷贝累计
/// 超此上限（等价 64KiB）即从最旧逐出（与事务数上限并列的硬编码期望）。
pub const UNDO_BYTE_CAP: usize = 64 * 1024;

/// 撤销合并时间窗：两次编辑间隔 < 300ms 才并入同一事务；停顿 ≥ 该值即新起撤销单元
/// （zed/VS Code 式分组直觉：持续打字合并、停顿后打断）。
pub const UNDO_GROUP_MS: Duration = Duration::from_millis(300);

/// 编辑意图（决定合并规则）：自 `base` 下沉（断 editor↔ui、editor↔ime
/// 两个耦合环），此处 re-export 保持 `crate::editor::EditIntent` 兼容。
pub use base::EditIntent;

/// 撤销单元合并判定（editor `UndoManager::push_edit` 与 wysiwyg
/// `edit.rs::record_undo_unit` 共用；收敛自两处各自内联的判定，Batch E / P13）。
///
/// 纯判定、无状态、无存储：返回 `true` = 并入当前打开的撤销单元（调用方按
/// true/false 走各自存储分支）。
///
/// 两处判定逐行对照（Batch E / P13）确认的**公共子集**仅此组合，参数即分量：
/// - `intent`：`Atomic` 永不合并（两处同一规则）。
/// - `adjacent`：调用方按**各自语义**预判的「同意图 + 邻接几何」；意图守恒两处同真
///   （editor pending 事务内意图锁定；wysiwyg 判 `k == intent`），邻接量则不同（见下）
///   → 由调用方算好传 bool，本函数不再重判。
/// - `since_last_ms`：I1 时间窗，距上次编辑 < [`UNDO_GROUP_MS`]（300ms）才并入；
///   无上一编辑时传 0（此时 `adjacent` 必为 false，本值不影响结论）。
/// - `cooperative`：editor 持久标志（光标移动 `set_cooperative(false)` 断层）；
///   wysiwyg 无此标志、光标移动直接置空 `last_edit` 等效断层 → 恒传 `true`。
///
/// **已知差异（不强行统一，各自保留）**：
/// - cooperative 载体：editor 标志；wysiwyg 无标志（置空 `last_edit` 等效断层）。
/// - Typing 邻接量：editor `next.start == last.start + last.new_text.len()`（**上一插入长**）；
///   wysiwyg `r.start == s + inserted_len`（**当前插入长**）。多字节字符 / 粘贴等
///   变长插入下两者不等价。
/// - 纯插入无换行门：editor 经 `Edit::inserts_newline()` 拒 `\n`/`\r`；wysiwyg 分类只查
///   `!text.contains('\n')`（孤 `\r` 插段会被 wysiwyg 判为续打）。
/// - Backspace / Delete 邻接量逐字节等价（当前删段 `end==prev.start` / `start==prev.start`）。
pub fn should_merge_undo(intent: EditIntent, adjacent: bool, since_last_ms: u64, cooperative: bool) -> bool {
    if intent == EditIntent::Atomic {
        return false;
    }
    cooperative
        && adjacent
        && since_last_ms < UNDO_GROUP_MS.as_millis() as u64
}

/// 一个已提交的撤销/重做单元。
#[derive(Clone, Debug)]
pub struct Transaction {
    pub intent: EditIntent,
    pub edits: Vec<Edit>,
    /// 事务开始前的选区 `(anchor, head)`。
    pub before: (usize, usize),
    /// 事务结束后的选区 `(anchor, head)`。
    pub after: (usize, usize),
}

#[derive(Clone, Debug)]
struct PendingTx {
    intent: EditIntent,
    edits: Vec<Edit>,
    before: (usize, usize),
}

/// 撤销栈。文本本身的回放由调用方用 [`crate::editor::buffer::Buffer`] 的
/// `undo_edit`/`redo_edit` 执行，本对象只负责分组、进出栈与选区快照。
///
/// `undo_stack` 用 [`VecDeque`]：超出上限丢**最早**（`pop_front` 删头 O(1)，Vec 的
/// `remove(0)` 是 O(cap) 整段搬移）；`redo_stack` 只需压尾/弹尾（`push`/`pop`），
/// 用 Vec 即可，不引入无关结构。
#[derive(Default)]
pub struct UndoManager {
    undo_stack: VecDeque<Transaction>,
    redo_stack: Vec<Transaction>,
    pending: Option<PendingTx>,
    cooperative: bool,
    /// 上一次编辑/合并发生的时刻（I1 时间窗）：自它停顿 ≥[`UNDO_GROUP_MS`] 未编辑 →
    /// 下一条不再并入当前事务（新起撤销单元）。`None` = 尚无编辑。
    last_edit_at: Option<Instant>,
}

impl UndoManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始/续接一个事务。若已存在同意图的**可合作**事务且距上一编辑 <[`UNDO_GROUP_MS`]
    /// 则续接并返回 `true`；否则先提交旧事务、开启新事务。`before`：本事务开始前的选区。
    pub fn start_tx(&mut self, intent: EditIntent, before: (usize, usize)) -> bool {
        let can_continue = self
            .pending
            .as_ref()
            .map(|p| {
                p.intent == intent
                    && self.cooperative
                    // I1 时间窗：停顿 ≥ UNDO_GROUP_MS → 视为新事务起点（刷新 before）。
                    && self.last_edit_at.is_none_or(|t| t.elapsed() < UNDO_GROUP_MS)
            })
            .unwrap_or(false);
        if can_continue {
            return true;
        }
        self.commit_pending(None);
        // P0：撤销后再编辑 = 编辑历史分叉，重做分支整棵作废。此处开启全新事务
        // （pending 由 None→Some）→ 清空 redo_stack，防 Ctrl+Y 把旧事务以旧坐标
        // 重放到已含新编辑的缓冲区（`commit_pending` 只在其内真实提交时清，直接
        // start_tx 且 pending=None / 空 pending 时靠这里清）。
        self.redo_stack.clear();
        self.pending = Some(PendingTx {
            intent,
            edits: Vec::new(),
            before,
        });
        false
    }

    /// 把一条编辑记入当前事务（必要时先刷新出独立事务）。
    pub fn push_edit(&mut self, edit: Edit) {
        // 兜底：未 start_tx 直接 push → 原子事务
        if self.pending.is_none() {
            // P0：未走 start_tx 的新编辑同样作废重做分支（pending 由 None→Some）。
            self.redo_stack.clear();
            self.pending = Some(PendingTx {
                intent: EditIntent::Atomic,
                edits: Vec::new(),
                before: (0, 0),
            });
        }

        // 合并判定（Batch E / P13 收敛至 `should_merge_undo`，与 wysiwyg edit.rs 共用）：
        // editor 侧 pending 事务内的编辑恒定同意图（intent 在 tx 建立时锁定）→ 邻接仅剩
        // 几何判定（`try_merge`）；cooperative 门 / I1 时间窗由共享判定统一裁决。
        // 克隆末条以避免与可变借用冲突；合并频率低、Edit 很小。
        let intent = self.pending.as_ref().unwrap().intent;
        let last_clone = self.pending.as_ref().and_then(|p| p.edits.last().cloned());
        let merge_ready =
            last_clone.as_ref().and_then(|last| self.try_merge(intent, last, &edit));
        let can_merge = should_merge_undo(
            intent,
            merge_ready.is_some(),
            self.last_edit_at.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0),
            self.cooperative,
        );
        if can_merge
            && let Some(merged) = merge_ready
            && let Some(p) = self.pending.as_mut()
            && let Some(slot) = p.edits.last_mut() {
                *slot = merged;
                self.last_edit_at = Some(Instant::now());
                return;
            }

        // 不合并 → 刷新旧事务，新事务沿用同意图（连续删除/粘贴的下一段），把本条放入。
        let before = self.pending.as_ref().unwrap().before;
        self.commit_pending(None);
        // P0：刷新后开启的新事务 = 新用户编辑，同样作废重做分支（`commit_pending`
        // 只在其内真实提交时清 redo；此处对「空 pending 早退」情形补一刀，语义完备）。
        self.redo_stack.clear();
        self.pending = Some(PendingTx {
            intent,
            edits: vec![edit],
            before,
        });
        self.last_edit_at = Some(Instant::now());
    }

    /// 按意图尝试把 `next` 并入 `last`（纯几何判定）：可并入返回合并后的 `last`。
    /// cooperative 门 / I1 时间窗由调用方 `should_merge_undo` 统一裁决（Batch E / P13）。
    fn try_merge(&self, intent: EditIntent, last: &Edit, next: &Edit) -> Option<Edit> {
        match intent {
            EditIntent::Typing => {
                // 连续打字：两侧纯插入、无换行、且紧邻（next 起点 = last 插入后终点）。
                if last.old_text.is_empty()
                    && next.old_text.is_empty()
                    && !last.inserts_newline()
                    && !next.inserts_newline()
                    && next.start == last.start + last.new_text.len()
                {
                    let mut merged = last.clone();
                    let text = format!("{}{}", last.new_text, next.new_text);
                    merged.new_text = text.into();
                    Some(merged)
                } else {
                    None
                }
            }
            EditIntent::Backspace => {
                // 纯删除且首尾相接（next 删除紧贴在 last 删除之前）。
                if last.new_text.is_empty()
                    && next.new_text.is_empty()
                    && next.start + next.old_text.len() == last.start
                {
                    let mut merged = next.clone();
                    let text = format!("{}{}", next.old_text, last.old_text);
                    merged.old_text = text.into();
                    Some(merged)
                } else {
                    None
                }
            }
            EditIntent::Delete => {
                // 纯删除且同起点 → 向长延伸。
                if last.new_text.is_empty()
                    && next.new_text.is_empty()
                    && next.start == last.start
                {
                    let mut merged = last.clone();
                    let text = format!("{}{}", last.old_text, next.old_text);
                    merged.old_text = text.into();
                    Some(merged)
                } else {
                    None
                }
            }
            EditIntent::Atomic => None,
        }
    }

    /// 提交遗留事务（记录 `after` 选区，入撤销栈、清空重做栈）。
    /// `cooperative` 保持原值（commit 由动作边界调用，会再显式 set_cooperative）。
    pub fn commit(&mut self, after: (usize, usize)) {
        self.commit_pending(Some(after));
    }

    /// 是否允许后续编辑与当前事务合并（光标移动/点击等置 `false`）。
    pub fn set_cooperative(&mut self, cooperative: bool) {
        self.cooperative = cooperative;
    }

    fn commit_pending(&mut self, after: Option<(usize, usize)>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.edits.is_empty() {
            return;
        }
        let after = after.unwrap_or_else(|| {
            let last = pending.edits.last().expect("非空");
            let pos = last.start + last.new_text.len();
            (pos, pos)
        });
        let tx = Transaction {
            intent: pending.intent,
            edits: pending.edits,
            before: pending.before,
            after,
        };
        self.undo_stack.push_back(tx);
        if self.undo_stack.len() > MAX_UNDO_TXS {
            // 丢最早：VecDeque 删头 O(1)（原 Vec::remove(0) 是 O(cap) 整段搬移）。
            self.undo_stack.pop_front();
        }
        // 字节上限（累计每条 Edit 的 old+new 全量拷贝）：与事务数 cap 同口径，
        // 超限从最旧逐出；单条超限大事务至少保留一条（防空栈失守）。
        let mut total: usize = self
            .undo_stack
            .iter()
            .flat_map(|t| t.edits.iter())
            .map(|e| e.old_text.len() + e.new_text.len())
            .sum();
        while total > UNDO_BYTE_CAP && self.undo_stack.len() > 1 {
            if let Some(evicted) = self.undo_stack.pop_front() {
                total = total.saturating_sub(
                    evicted
                        .edits
                        .iter()
                        .map(|e| e.old_text.len() + e.new_text.len())
                        .sum(),
                );
            }
        }
        self.redo_stack.clear();
    }

    /// 提交遗留事务并撤销一步。返回该事务（供调用方回放 & 恢复 `before` 选区）。
    pub fn undo(&mut self) -> Option<Transaction> {
        self.commit_pending(None);
        self.cooperative = false;
        let tx = self.undo_stack.pop_back()?;
        self.redo_stack.push(tx.clone());
        Some(tx)
    }

    /// 重做一步。返回该事务（供调用方回放 & 恢复 `after` 选区）。
    pub fn redo(&mut self) -> Option<Transaction> {
        // P0 防御：先提交遗留 pending（含空 pending 早退）再弹 redo——不丢 pending、
        // 也防旧事务以旧坐标重放到未提交的新改动上。正常路径下新编辑已把 redo_stack
        // 清空（见 start_tx/push_edit），此处 pop 返回 None；撤销后未编辑直接 Ctrl+Y
        // 时 pending 为 None，行为不变。
        self.commit_pending(None);
        self.cooperative = false;
        let tx = self.redo_stack.pop()?;
        self.undo_stack.push_back(tx.clone());
        Some(tx)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
            || self.pending.as_ref().is_some_and(|p| !p.edits.is_empty())
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 清空全部（切源/重置时）。
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.pending = None;
        self.cooperative = false;
        self.last_edit_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::buffer::Buffer;

    /// P0 红测：撤销后开始新编辑（事务 B 未 commit）→ redo 必须已失效（返回 None）。
    ///
    /// 语义（浏览器/编辑器通例）：撤销后再编辑 = 重做分支整棵作废。复现路径：
    /// 输入 abc（合并为事务 A）→ commit → undo（此刻 undo_stack=[], redo_stack=[A],
    /// pending=None）→ 再输入 x 开启新事务 B——`start_tx` 因 pending=None 走
    /// `commit_pending(None)` 早退，**不清 redo_stack**，于是 `redo()` 仍从 redo_stack
    /// 弹回旧事务 A，以旧坐标 A 重放到已含 "x" 的缓冲区 → 文本损坏/选区错乱。
    ///
    /// 断言：新事务 B 开启后 `redo()` 返回 None。修复前返回 Some(A) → 红。
    #[test]
    fn redo_invalidated_by_new_edit_after_undo() {
        let mut buf = Buffer::default();
        let mut mgr = UndoManager::new();
        // 输入 abc（合并成事务 A）。
        for (i, ch) in "abc".chars().enumerate() {
            mgr.start_tx(EditIntent::Typing, (i, i));
            mgr.push_edit(buf.apply_edit(i..i, &ch.to_string()));
            mgr.set_cooperative(true);
        }
        assert_eq!(buf.to_string(), "abc");
        mgr.commit((3, 3));

        // 撤销 A → undo_stack=[], redo_stack=[A], pending=None。
        let tx = mgr.undo().expect("撤销返回事务 A");
        assert_eq!(tx.edits.len(), 1);
        for e in tx.edits.iter().rev() {
            buf.undo_edit(e);
        }
        assert_eq!(buf.to_string(), "");
        assert!(mgr.can_redo(), "撤销后此刻应可重做（修前基线，redo_stack=[A]）");

        // 撤销后开始新编辑 x：开启事务 B，此刻重做分支应整个作废。
        mgr.start_tx(EditIntent::Typing, (0, 0));
        mgr.push_edit(buf.apply_edit(0..0, "x"));

        // 目标语义：redo() 必须为 None。修复前 start_tx 未清 redo_stack → 返回 Some(A) → 红。
        assert!(
            mgr.redo().is_none(),
            "撤销后新编辑应作废重做分支，实测 redo 仍弹回旧事务 A"
        );
    }

    /// M 红测：undo 栈只限事务数（[`MAX_UNDO_TXS`]）不限字节——每条 `Edit` 持有 old+new
    /// 全量拷贝，连续大块插入（反复粘贴大段文本）驻留近 N× 文档大小，内存无界。
    ///
    /// 期望语义：undo 栈累计字节受字节上限约束（与事务数 cap 同口径），超限从最旧逐出。
    /// 当前无字节上限 → 20 事务累计远超下限 → 红。
    #[test]
    fn undo_stack_caps_total_bytes() {
        // 字节上限口径：与既有事务数 cap 并列的硬编码期望（实现将落同名常量）。
        const UNDO_BYTE_CAP: usize = 64 * 1024;
        // 每事务插入 20KB（纯插入，old_text 为空）；20 个原子事务各成独立撤销单元。
        let chunk: String = "x".repeat(20 * 1024);
        let mut buf = Buffer::new("");
        let mut mgr = UndoManager::new();
        for _ in 0..20usize {
            let start = buf.len();
            mgr.start_tx(EditIntent::Atomic, (start, start));
            mgr.push_edit(buf.apply_edit(start..start, &chunk));
            mgr.commit((start + chunk.len(), start + chunk.len()));
        }
        assert_eq!(buf.len(), 20 * chunk.len());

        // undo 栈累计字节（每条 Edit 的 old+new 全量拷贝之和）。
        let total: usize = mgr
            .undo_stack
            .iter()
            .flat_map(|tx| tx.edits.iter())
            .map(|e| e.old_text.len() + e.new_text.len())
            .sum();
        assert!(
            total <= UNDO_BYTE_CAP,
            "undo 栈累计 {} 字节超出字节上限 {}，内存无界（应超限从最旧逐出）",
            total,
            UNDO_BYTE_CAP
        );
    }
}
