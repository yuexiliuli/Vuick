//! 启动 → 首帧墙钟诊断（`VUICK_STARTUP_DIAG=1` 开启；默认关闭零开销、零行为变化）。
//!
//! 为「轨 A 改造前基线」量主线程关键段墙钟：进程入口 [`boot`] 锚定起点，
//! `open_window` 回调 / `EditorApp::new` 结束（= `restore_session` 段）各 [`mark`] 一次，
//! 首次 render 时 [`summarize`] 向 stderr 打一行 `[STARTUP] boot=.. config=.. restore=..
//! firstframe=..`（对齐既有 `[TAG]` 谱系，数值 = 距进程入口的墙钟 ms）。
//! 未开 env 时全部函数每次调用仅一次 `var_os` 检查即返回，不分配、不动任何状态。

use std::cell::RefCell;
use std::time::Instant;

/// 诊断门控（每个函数入口一次 `var_os`；平时零开销）。
#[inline]
fn diag_on() -> bool {
    std::env::var_os("VUICK_STARTUP_DIAG").is_some()
}

struct Slot {
    boot: Instant,
    /// `open_window` 回调到达（config/prefs/键位已就绪）→ 距 boot 的 ms。
    config: Option<f64>,
    /// `restore_session` 段结束（含逐选项卡恢复）→ 距 boot 的 ms。
    restore: Option<f64>,
    /// 首次 render → 距 boot 的 ms。
    firstframe: Option<f64>,
}

thread_local! {
    static SLOT: RefCell<Option<Slot>> = const { RefCell::new(None) };
    /// 独立于可消费 [`SLOT`] 的进程入口时刻：`summarize` 会 `take` 掉 SLOT，首帧**之后**
    /// 的事件（如渐进会话恢复完成）仍需按 boot 基准补打墙钟 → 单独保存一份。
    static BOOT: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

/// 进程主线程入口最先调用：锚定 boot 起点。未开启时 no-op。
#[inline]
pub fn boot() {
    if !diag_on() {
        return;
    }
    BOOT.with(|b| *b.borrow_mut() = Some(Instant::now()));
    SLOT.with(|s| {
        *s.borrow_mut() = Some(Slot {
            boot: Instant::now(),
            config: None,
            restore: None,
            firstframe: None,
        })
    });
}

/// 记录一个分段「距 boot 的墙钟 (ms)」；同名分段只记首次（幂等，防 render 重复刷新）。
#[inline]
pub fn mark(segment: &'static str) {
    if !diag_on() {
        return;
    }
    SLOT.with(|s| {
        if let Some(slot) = s.borrow_mut().as_mut() {
            let ms = slot.boot.elapsed().as_secs_f64() * 1000.0;
            match segment {
                "config" => slot.config.get_or_insert(ms),
                "restore" => slot.restore.get_or_insert(ms),
                "firstframe" => slot.firstframe.get_or_insert(ms),
                _ => return,
            };
        }
    });
}

/// 首次 render 处调用：补记 firstframe 段（未显式 mark 过则按当前时刻）并打印一行
/// 汇总（幂等：已打印则槽位被取走，后续调用 no-op）。
#[inline]
pub fn summarize() {
    if !diag_on() {
        return;
    }
    SLOT.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(slot) = s.as_mut() {
            slot.firstframe
                .get_or_insert_with(|| slot.boot.elapsed().as_secs_f64() * 1000.0);
        }
        if let Some(slot) = s.take() {
            let fmt = |v: Option<f64>| match v {
                Some(ms) => format!("{ms:.1}ms"),
                None => "-".to_string(),
            };
            eprintln!(
                "[STARTUP] boot=0.0ms config={} restore={} firstframe={}",
                fmt(slot.config),
                fmt(slot.restore),
                fmt(slot.firstframe),
            );
        }
    });
}

/// 距进程入口的墙钟 (ms)；诊断未开启或从未 [`boot`] → None。
/// 供首帧**之后**才发生的事件（如渐进会话恢复完成）补打墙钟——`[STARTUP]` 汇总行固定
/// 在首帧打印（`summarize` 消费 SLOT），恢复完成属于其后，无法并入汇总行，改由消费方
/// 用本函数取 boot 基准自行补一行 stderr。读数来自独立的 [`BOOT`]（不受 summarize 消费）。
#[inline]
pub fn elapsed_ms() -> Option<f64> {
    if !diag_on() {
        return None;
    }
    BOOT.with(|b| {
        b.borrow()
            .as_ref()
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
    })
}

/// 清空诊断槽（测试/基准在同一进程内多次 `boot` 复用；未开启 no-op）。
#[allow(dead_code)]
pub fn reset() {
    BOOT.with(|b| *b.borrow_mut() = None);
    SLOT.with(|s| *s.borrow_mut() = None);
}
