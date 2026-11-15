//! 光标闪烁相位状态机（本批 Pattern 1）。
//!
//! 收敛自 `editor/src/view/interaction.rs`（`CodeEditor`）与
//! `markdown_wysiwyg/src/wysiwyg/core.rs`（`WyEditor`）两份逐行同构实现：状态字段组 +
//! `start_blink` 循环 + `kick_caret` + `caret_shown_now` + 常量（`BLINK_MS=500` /
//! `BLINK_PAUSE_MS=300`）全部下沉本模块，两个 View 只留薄调用，行为逐字节等价。
//! 常量改名 `CARET_BLINK_MS` / `CARET_BLINK_PAUSE_MS`（对齐 `ui/input.rs` 第三份拷贝，
//! 本批明确留作后续，不同步收敛）。
//!
//! # 保留的两处语义差异（参数/单行策略，禁止抹平）
//!
//! 1. **翻转 guard**：editor 翻转前 `if !self.focused { return; }`（省失焦后每 500ms
//!    一帧整树重建，见 interaction.rs:67-72）；wysiwyg 无条件翻转（core.rs:845-848）。
//!    → `CaretPhase` 构造参数 `flip_when_blurred`（editor=`false` / wysiwyg=`true`），
//!    [`CaretPhase::maybe_flip`] 据该字段与 `focused` 联合判定。
//! 2. **on_blur 复位**：wysiwyg `on_blur` 置 `caret_visible = true`（core.rs:822-826）；
//!    editor `on_blur` 不清 `caret_visible`（interaction.rs:45-49 尾段）。→ 该策略行
//!    留在各自 `on_blur` 里（在其它失焦逻辑旁逐字搬），不并入状态机。

use std::time::{Duration, Instant};

use gpui::Context;

/// 光标闪烁半周期（ms，对齐原 `BLINK_MS`）。
pub const CARET_BLINK_MS: u64 = 500;
/// 输入后光标暂停闪烁（实心显示，ms，对齐原 `BLINK_PAUSE_MS`）。
pub const CARET_BLINK_PAUSE_MS: u64 = 300;

/// 光标闪烁相位状态机：`focused` / `caret_visible` / `caret_pause_until` /
/// `blink_epoch` / `blink_running` 字段组 + `start_blink` 循环 + `kick_caret` +
/// `caret_shown_now` + 翻转 / keep 判定（本批 Pattern 1 收敛目标，见模块头）。
pub struct CaretPhase {
    /// 窗口/文档焦点态：两个 View 的 `on_focus`/`on_blur` 写，`caret_shown_now` 与
    /// blink keep 判定读。
    pub focused: bool,
    /// 光标可见相位（paint 消费 `caret_shown_now` 的唯一事实源）。
    pub caret_visible: bool,
    /// 交互静止期起点：点击/键移/输入后先亮住 `CARET_BLINK_PAUSE_MS` 再继续闪。
    pub caret_pause_until: Option<Instant>,
    /// 闪烁循环是否在跑（`begin_blink` 防重入 + 各处 `if !blink_running` 门）。
    pub blink_running: bool,
    /// 闪烁循环代号：`begin_blink` 每启动一次 `wrapping_add(1)`，循环体只认自己这拍
    /// 代号（[`CaretPhase::keep_running`]），旧代号随新启动自然失效。
    blink_epoch: u64,
    /// 是否「失焦仍翻转」（editor=`false` / wysiwyg=`true`；两处语义差异之一，见模块头）。
    flip_when_blurred: bool,
}

impl CaretPhase {
    /// 每 View 构造时定策略：`flip_when_blurred` = editor `false` / wysiwyg `true`。
    /// 其余字段全取「未聚焦、不闪」默认（对齐两处 `new()` 的逐字初值）。
    pub fn new(flip_when_blurred: bool) -> Self {
        Self {
            focused: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            blink_running: false,
            flip_when_blurred,
        }
    }

    /// 光标交互后常亮静止期（点击/键移/输入后先亮住再继续闪）。
    pub fn kick(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    /// 当前帧光标是否应绘制：失焦不画；交互静止期内常亮；否则跟随闪烁翻转。
    /// paint 的唯一光标相位（`caret_visible` 翻转由 blink 定时器驱动，非每帧计算）。
    pub fn caret_shown_now(&self) -> bool {
        if !self.focused {
            return false;
        }
        let paused = self
            .caret_pause_until
            .is_some_and(|p| p.elapsed() < Duration::from_millis(CARET_BLINK_PAUSE_MS));
        paused || self.caret_visible
    }

    /// 闪烁循环的「每拍翻转」：静止期结束后（`elapsed >= CARET_BLINK_PAUSE_MS`）且
    /// `flip_when_blurred || focused` 才翻转；返回「是否该 notify」（= 确实翻转了）。
    ///
    /// 语义差异 1（见模块头）：editor `flip_when_blurred=false` + 失焦 → 不翻不
    /// notify（对齐原 `if !self.focused { return; }`，省失焦后每 500ms 一帧整树重建，
    /// 且重获焦点时 `on_focus` 已重置 `caret_visible=true`）；wysiwyg
    /// `flip_when_blurred=true` → 无条件翻转 + notify。
    pub fn maybe_flip(&mut self) -> bool {
        let elapsed = self
            .caret_pause_until
            .map_or(u128::MAX, |p| p.elapsed().as_millis());
        if elapsed >= CARET_BLINK_PAUSE_MS as u128 && (self.flip_when_blurred || self.focused) {
            self.caret_visible = !self.caret_visible;
            true
        } else {
            false
        }
    }

    /// 防重入地启动一次闪烁：已在跑（`blink_running`）→ `None`；否则置运行态、
    /// `blink_epoch` 自增并返回本拍代号（循环体以该代号做 keep 判定）。
    /// 必须由宿主在 `spawn` 前的 `&mut self` 上下文同步调用（既有 blink 测试在
    /// `spawn` 后立即读 `blink_running`）。
    pub fn begin_blink(&mut self) -> Option<u64> {
        if self.blink_running {
            return None;
        }
        self.blink_running = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        Some(self.blink_epoch)
    }

    /// blink 循环体的继续判定：持焦且代号未被更新（= 该拍仍是最新一次启动）。
    pub fn keep_running(&self, epoch: u64) -> bool {
        self.focused && epoch == self.blink_epoch
    }

    /// blink 循环退出时复位运行态（放行下一次 `begin_blink`）。
    pub fn stop(&mut self) {
        self.blink_running = false;
    }
}

/// 宿主实体（`CodeEditor` / `WyEditor`）暴露可变光标相位状态的统一入口，供
/// [`spawn_blink_loop`] 的循环体经 `Entity` 更新访问。
pub trait CaretOwner {
    /// 可变访问光标相位状态机。
    fn caret(&mut self) -> &mut CaretPhase;
}

/// 单一闪烁循环实现：`cx.spawn(loop { ... }).detach()`——interaction.rs:51-89 与
/// wysiwyg/core.rs:828-862 的公共骨架逐字搬：
/// - 首读静止期剩余：`elapsed >= CARET_BLINK_PAUSE_MS` 才进翻转；
///   editor 的 `if !focused { return; }` 被 [`CaretPhase::maybe_flip`] 内策略吸收；
/// - 翻转成立才 `cx.notify()`（对齐原「实体 notify → gpui 按访问追踪作废宿主」）；
/// - 停 `CARET_BLINK_MS` 后判 keep：**持焦且代号未过期**（两道失效源：失焦 /
///   新一次 `begin_blink`）；不 keep 才 `stop()` 并退出。
///
/// `epoch` 由宿主在 [`CaretPhase::begin_blink`] **同步**取得后传入，保证
/// `blink_running` / 代号在 `spawn` 瞬间置位（既有 blink 测试读 `blink_running`）。
pub fn spawn_blink_loop<C: CaretOwner + 'static>(epoch: u64, cx: &mut Context<C>) {
    cx.spawn(async move |this, cx| loop {
        let elapsed = this
            .update(cx, |this, _| {
                this.caret()
                    .caret_pause_until
                    .map_or(u128::MAX, |p| p.elapsed().as_millis())
            })
            .unwrap_or(u128::MAX);
        if elapsed >= CARET_BLINK_PAUSE_MS as u128 {
            let _ = this.update(cx, |this, cx| {
                if this.caret().maybe_flip() {
                    cx.notify();
                }
            });
        }
        cx.background_executor()
            .timer(Duration::from_millis(CARET_BLINK_MS))
            .await;
        let keep = this
            .update(cx, |this, _| this.caret().keep_running(epoch))
            .unwrap_or(false);
        if !keep {
            this.update(cx, |this, _| this.caret().stop()).ok();
            break;
        }
    })
    .detach();
}
