//! 性能剖析微工具（`KILO_PROF=1` 开启）。
//!
//! 本模块只做「累计各标签耗时 + 周期输出」，放在被剖析的热点里；开启时每阶段打印
//! 各标签的累计耗时/次数，便于在模拟真实编辑的测试里拆解卡顿来源。
//! 不开 `KILO_PROF` 时，`end/add_absolute` 只做一次借锁检查（可忽略）。
//!
//! 操作级实时追踪（真实界面操作计时）：`WY_TRACE=<path>` 开启。把
//! [`op_start`] 挂在输入/删除/滚动等**操作入口**，把 [`op_finish_after_paint`]
//! 挂在对应元素 `paint` 末尾——每次操作结算一行 `OP ... kind total=<ms> | <阶段
//! breakdown>` 到文件（未设路径则打到 stderr）。`total` 是「操作开始 → 该操作画面
//! 绘制完成」的感知延迟（墙钟），breakdown 是同一操作帧内各标签的累计耗时。
//! 不设 `WY_TRACE` 时全部为一次线程局部借锁检查（可忽略）。

use std::cell::RefCell;
use std::io::Write;
use std::time::{Duration, Instant};

struct Prof {
    on: bool,
    acc: std::collections::HashMap<&'static str, (u64, Duration)>,
}

thread_local! {
    static P: RefCell<Prof> = RefCell::new(Prof {
        on: std::env::var_os("KILO_PROF").is_some() || std::env::var_os("WY_TRACE").is_some(),
        acc: Default::default(),
    });
}

/// 取当前累计标签行（按总时长降序），不清空。
fn snapshot_rows() -> Vec<(&'static str, u64, Duration)> {
    P.with(|p| {
        let p = p.borrow();
        let mut rows: Vec<(&'static str, u64, Duration)> = p
            .acc
            .iter()
            .map(|(k, (n, d))| (*k, *n, *d))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.2));
        rows
    })
}

fn clear_acc() {
    P.with(|p| p.borrow_mut().acc.clear());
}

// ---------------------------------------------------------------------------
// 操作级实时追踪（WY_TRACE）
// ---------------------------------------------------------------------------

struct OpState {
    seq: u64,
    kind: &'static str,
    start: Instant,
    /// paint 改动行/带等现场信息（调试排查用，可选）。
    detail: String,
    /// op 开始时的全局 paint 计数（排查一次操作是否跨多帧/paint）。
    paint_0: u64,
}

thread_local! {
    static OP: RefCell<Option<OpState>> = const { RefCell::new(None) };
    static TRACE_FILE: RefCell<Option<std::io::BufWriter<std::fs::File>>> = const { RefCell::new(None) };
    static OP_SEQ: RefCell<u64> = const { RefCell::new(0) };
    /// WyEditorElement::paint 的全局次数（跨 op 累计，供 paint 计数诊断）。
    static G_PAINT_N: RefCell<u64> = const { RefCell::new(0) };
}

/// WyEditorElement::paint 每次调用时递增（诊断一次操作跨几帧）。
pub fn paint_mark() {
    if !trace_gate() {
        return;
    }
    G_PAINT_N.with(|n| *n.borrow_mut() += 1);
}

fn paint_count() -> u64 {
    G_PAINT_N.with(|n| *n.borrow())
}

fn trace_gate() -> bool {
    std::env::var_os("WY_TRACE").is_some()
}

/// WY_TRACE 是否开启（供调用点避免为诊断字符串做无谓分配）。
pub fn trace_active() -> bool {
    std::env::var_os("WY_TRACE").is_some()
}

fn init_trace_file() {
    if TRACE_FILE.with(|f| f.borrow().is_some()) {
        return;
    }
    if let Some(path) = std::env::var_os("WY_TRACE") {
        let p = std::path::PathBuf::from(&path);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            Ok(f) => {
                TRACE_FILE.with(|t| *t.borrow_mut() = Some(std::io::BufWriter::new(f)));
            }
            Err(_) => {
                TRACE_FILE.with(|t| *t.borrow_mut() = None);
            }
        }
    }
}

fn write_trace_line(line: &str) {
    init_trace_file();
    let wrote = TRACE_FILE.with(|t| {
        if let Some(w) = t.borrow_mut().as_mut() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.write_all(b"\n");
            let _ = w.flush();
            true
        } else {
            false
        }
    });
    if !wrote {
        eprintln!("{line}");
    }
}

/// 当前是否有未结算的操作会话（供滚动/打开挂点判断：输入操作在途时不再
/// 重复起新会话，避免把输入帧误标成 scroll/open）。
pub fn op_pending() -> bool {
    if !trace_gate() {
        return false;
    }
    OP.with(|o| o.borrow().is_some())
}

/// 开始一个操作计时会话（输入/删除/滚动等）。若上一会话未在 `paint` 结算
/// （例如操作没触发重绘帧），在此紧急结算为 `dropped`，避免串账。
pub fn op_start(kind: &'static str) {
    if !trace_gate() {
        return;
    }
    // 上一未结算的操作：结算为 dropped（记录其累计那部分耗时，但标记未走完）。
    let prev = OP.with(|o| o.borrow_mut().take());
    if let Some(prev) = prev {
        let total = prev.start.elapsed();
        let rows = snapshot_rows();
        clear_acc();
        let mut line = format!(
            "OP seq={} kind={} status=dropped total={:.3}ms detail={}",
            prev.seq,
            prev.kind,
            total.as_secs_f64() * 1000.0,
            prev.detail,
        );
        if !rows.is_empty() {
            line.push_str(" | ");
            line.push_str(
                &rows
                    .iter()
                    .map(|(k, n, d)| format!(
                        "{k}={:.3}ms x{n}",
                        d.as_secs_f64() * 1000.0 / *n as f64
                    ))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        write_trace_line(&line);
    }
    OP_SEQ.with(|s| *s.borrow_mut() += 1);
    let seq = OP_SEQ.with(|s| *s.borrow());
    // 操作级 breakdown 要限定在本操作期间：清掉此前（上一帧/前一次操作）的累计。
    clear_acc();
    OP.with(|o| {
        *o.borrow_mut() = Some(OpState {
            seq,
            kind,
            start: Instant::now(),
            detail: String::new(),
            paint_0: paint_count(),
        })
    });
}

/// 带现场信息的 [`op_start`]（detail 用于排查：如编辑块/带范围）。
pub fn op_start_detail(kind: &'static str, detail: &str) {
    if !trace_gate() {
        return;
    }
    op_start(kind);
    OP.with(|o| {
        if let Some(s) = o.borrow_mut().as_mut() {
            s.detail = detail.to_string();
        }
    });
}

/// 向当前操作会话的 detail 追加现场信息（多个原因逗号分隔），供排查。
pub fn op_detail_append(tag: &str, v: &str) {
    if !trace_gate() {
        return;
    }
    OP.with(|o| {
        if let Some(s) = o.borrow_mut().as_mut() {
            if !s.detail.is_empty() {
                s.detail.push(' ');
            }
            s.detail.push_str(tag);
            s.detail.push('=');
            s.detail.push_str(v);
        }
    });
}

/// paint 末尾结算：当前操作从开始到本帧绘制完成的感知延迟 + 帧内阶段拆解。
/// 无挂起操作则为 no-op。
pub fn op_finish_after_paint() {
    if !trace_gate() {
        return;
    }
    let Some(op) = OP.with(|o| o.borrow_mut().take()) else {
        return;
    };
    let total = op.start.elapsed();
    let rows = snapshot_rows();
    clear_acc();
    let mut line = format!(
        "OP seq={} kind={} status=done total={:.3}ms paints={} detail={}",
        op.seq,
        op.kind,
        total.as_secs_f64() * 1000.0,
        paint_count().saturating_sub(op.paint_0),
        op.detail,
    );
    if !rows.is_empty() {
        line.push_str(" | ");
        line.push_str(
            &rows
                .iter()
                .map(|(k, n, d)| format!(
                    "{k}={:.3}ms x{n}",
                    d.as_secs_f64() * 1000.0 / *n as f64
                ))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    write_trace_line(&line);
}

#[allow(dead_code)]
pub fn enabled() -> bool {
    std::env::var_os("KILO_PROF").is_some()
}

/// 开（测试启动时调用一次；线程局部已按环境变量初始化）。
#[allow(dead_code)]
pub fn enable() {
    P.with(|p| p.borrow_mut().on = true);
}

/// 返回记录开始时刻；配合 [`end`] 使用。
pub fn begin() -> Instant {
    Instant::now()
}

/// 结算一个标签的耗时（仅开启时累计）。
pub fn end(label: &'static str, t: Instant) {
    let on = P.with(|p| p.borrow().on);
    if !on {
        return;
    }
    let d = t.elapsed();
    P.with(|p| {
        let mut p = p.borrow_mut();
        let e = p.acc.entry(label).or_insert((0, Duration::ZERO));
        e.0 += 1;
        e.1 += d;
    });
}

/// 直接累计一个外部测得的时长（如测试里测得的整段墙钟时间）。
#[allow(dead_code)]
pub fn add_absolute(label: &'static str, d: Duration) {
    let on = P.with(|p| p.borrow().on);
    if !on {
        return;
    }
    P.with(|p| {
        let mut p = p.borrow_mut();
        let e = p.acc.entry(label).or_insert((0, Duration::ZERO));
        e.0 += 1;
        e.1 += d;
    });
}

/// 打印当前累计（按总时长降序）并清空。
#[allow(dead_code)]
pub fn dump(tag: &str) {
    if !enabled() {
        return;
    }
    let mut rows: Vec<(&'static str, u64, Duration)> = P.with(|p| {
        p.borrow()
            .acc
            .iter()
            .map(|(k, (n, d))| (*k, *n, *d))
            .collect()
    });
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    eprintln!("===== [PROF] {tag} =====");
    if rows.is_empty() {
        eprintln!("  (无累计)");
    }
    for (k, n, d) in rows {
        eprintln!(
            "  {k:<22} {n:>6}x  {:>9.2} ms  {:>8.3} ms/次",
            d.as_secs_f64() * 1000.0,
            d.as_secs_f64() * 1000.0 / n.max(1) as f64,
        );
    }
    P.with(|p| p.borrow_mut().acc.clear());
}
