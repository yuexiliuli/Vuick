//! 撤销/重做：确定性事务合并 + 300ms 时间窗。
//!
//! 设计吸收 **gpui-component `UndoManager`**：
//! - 事务 = 「一组编辑 + 编辑前后选区」，编辑与光标绑定恢复（zed 也是「事务即编辑单元」）。
//! - **确定性合并 + 时间窗**：按 `EditIntent` + 邻接性确定性判定（连续打字合并；Backspace
//!   首尾相接合并；Delete 同点延伸合并；Atomic/换行永远独立）；**I1 时间窗**：与上一编辑
//!   停顿 ≥[`UNDO_GROUP_MS`]（300ms）即新起撤销单元（对齐 zed `History.group_interval`，
//!   std 单调钟纯时间判定、无定时器副作用）。光标移动即 `set_cooperative(false)` 断层。
//! - 上限 [`MAX_UNDO_TXS`] 个事务，超出丢最早。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::editor::buffer::Edit;

pub const MAX_UNDO_TXS: usize = 1000;

/// 撤销合并时间窗：两次编辑间隔 < 300ms 才并入同一事务；停顿 ≥ 该值即新起撤销单元
/// （zed/VS Code 式分组直觉：持续打字合并、停顿后打断）。
pub const UNDO_GROUP_MS: Duration = Duration::from_millis(300);

/// 编辑意图（决定合并规则）。
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
            self.pending = Some(PendingTx {
                intent: EditIntent::Atomic,
                edits: Vec::new(),
                before: (0, 0),
            });
        }

        // 尝试合并（同意图 + 时间窗 + 邻接性，见 `try_merge`；克隆末条以避免与可变借用
        // 冲突；合并频率低、Edit 很小）
        if self.cooperative {
            let intent = self.pending.as_ref().unwrap().intent;
            let last_clone = self.pending.as_ref().and_then(|p| p.edits.last().cloned());
            if let Some(last) = last_clone
                && let Some(merged) = self.try_merge(intent, &last, &edit)
                    && let Some(p) = self.pending.as_mut()
                        && let Some(slot) = p.edits.last_mut() {
                            *slot = merged;
                            self.last_edit_at = Some(Instant::now());
                            return;
                        }
        }

        // 不合并 → 刷新旧事务，新事务沿用同意图（连续删除/粘贴的下一段），把本条放入。
        let intent = self.pending.as_ref().unwrap().intent;
        let before = self.pending.as_ref().unwrap().before;
        self.commit_pending(None);
        self.pending = Some(PendingTx {
            intent,
            edits: vec![edit],
            before,
        });
        self.last_edit_at = Some(Instant::now());
    }

    /// 尝试把 `next` 并入 `last`；可并入返回合并后的 `last`。
    fn try_merge(&self, intent: EditIntent, last: &Edit, next: &Edit) -> Option<Edit> {
        // I1 时间窗：与上一编辑停顿 ≥ UNDO_GROUP_MS → 不并入（超窗断事务）。
        if self.last_edit_at.is_none_or(|t| t.elapsed() >= UNDO_GROUP_MS) {
            return None;
        }
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
