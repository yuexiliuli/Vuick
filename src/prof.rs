//! 性能剖析微工具（`KILO_PROF=1` 开启）。
//!
//! 本模块只做「累计各标签耗时 + 周期输出」，放在被剖析的热点里；开启时每阶段打印
//! 各标签的累计耗时/次数，便于在模拟真实编辑的测试里拆解卡顿来源。
//! 不开 `KILO_PROF` 时，`end/add_absolute` 只做一次借锁检查（可忽略）。

use std::cell::RefCell;
use std::time::{Duration, Instant};

struct Prof {
    on: bool,
    acc: std::collections::HashMap<&'static str, (u64, Duration)>,
}

thread_local! {
    static P: RefCell<Prof> = RefCell::new(Prof {
        on: std::env::var_os("KILO_PROF").is_some(),
        acc: Default::default(),
    });
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
    rows.sort_by(|a, b| b.2.cmp(&a.2));
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
