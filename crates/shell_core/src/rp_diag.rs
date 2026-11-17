//! 阅读进度诊断探针（真机追查「md 用【打开最近】重开不复位」的证据通道，K46）。
//!
//! - 默认关闭：`VUICK_RP_DIAG=1` 才启用；开关/测试判定/路径收敛自 `base::diag`
//!   （本批 Pattern 7），不进入热路径。
//! - 只在**事件点**调用（save / apply / apply_text_restore / check 置脏 / 预览收敛），
//!   不逐帧刷屏。
//! - 追加写 `%APPDATA%\vuick\rp_diag.log`（测试构建写 `%TEMP%\vuick-rp-diag.log`，
//!   不污染真实 APPDATA；`reading_progress` 测试同款隔离姿势）。旧实现生产侧曾取
//!   `UserPrefs::config_path().parent()`（`VUICK_CONFIG` 覆盖时 ≠ APPDATA）；收敛到
//!   `base::diag::diag_log_path` 的统一 APPDATA 口径后与 `editor::diag` 同址、行可交错归因。
//! - 结束运行**不清理**，保留供主 agent 读取；`.gitignore` 收 `rp_diag.log` 不入库。
//! - 与 `AppHost::is_test_build` 同口径判定测试构建（本模块无 host，运行时判 env）。

use session_store::ReadingProgress;

/// 探针是否开启（进程级缓存一次判读）：`VUICK_RP_DIAG=1`；其余值 / 缺失 → 关。
/// 开关/测试判定/路径收敛自 `base::diag`（本批 Pattern 7）。
pub fn enabled() -> bool {
    base::diag::env_flag("VUICK_RP_DIAG")
}

/// 紧凑描述一份阅读进度值（`key=<k>` 后的值段，区分 md / epub / pdf）。
pub fn rp_value(rp: &ReadingProgress) -> String {
    match rp {
        ReadingProgress::Markdown { top_line } => format!("md top_line={top_line}"),
        ReadingProgress::Epub { top_line } => format!("epub line={top_line}"),
        ReadingProgress::Pdf { page_index, in_page_frac, zoom } => format!(
            "pdf page={page_index} frac={in_page_frac:.4} zoom={zoom:.4}"
        ),
    }
}

/// 描述一个可空的 (idx, 位置) 哨兵（`check` 脏哨兵比对的 old/new 段）。
pub fn pos_value(p: &Option<(usize, Option<ReadingProgress>)>) -> String {
    match p {
        Some((i, Some(v))) => format!("idx={i} {}", rp_value(v)),
        Some((i, None)) => format!("idx={i} pos=none"),
        None => "sentinel=none".to_string(),
    }
}

/// 写入一行 `[RP] <ts_ms> <event> <detail>`（追加、UTF-8；写失败静默丢弃，绝不影响主流程）。
/// 行首时间戳取启动后相对毫秒（等价于 `Instant`，供同一进程内定位事件顺序）。
pub fn diag(event: &str, detail: &str) {
    if !enabled() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[RP] {ts} {event} {detail}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base::diag::diag_log_path("rp_diag.log"))
    {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
}

/// 「md 预览收敛」延迟上报的状态：`apply` 命中 Markdown 后登记，逐渲染帧计数，到
/// 2 帧后把右栏预览 `top_source_line` 落日志（验证预览是否随编辑区恢复收敛）。
/// 进程级静态即可（单窗口主线程），免改 EditorApp 结构。
static PREVIEW_PENDING: std::sync::Mutex<Option<(String, u32)>> = std::sync::Mutex::new(None);

/// `apply_reading_progress` 命中 Markdown → 登记延迟上报（key + 帧计数起点）。
pub fn mark_preview_pending(key: String) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = PREVIEW_PENDING.lock() {
        *g = Some((key, 0));
    }
}

/// 每渲染帧调用一次（shell `render` 中 sync 之后）：登记未满 2 帧则推进；满 2 帧时
/// 调用方已把「当前预览顶源码行」经 [`Self::preview_key_for_report`] / 直接在此上报——
/// 本函数负责帧计数与清除，避免 shell 侧感知模块内部状态。
pub fn preview_pending_due() -> Option<String> {
    if !enabled() {
        return None;
    }
    let mut g = PREVIEW_PENDING.lock().ok()?;
    let (key, n) = g.as_ref()?;
    let n = n + 1;
    if n < 2 {
        *g = Some((key.clone(), n));
        return None;
    }
    let key = g.take().map(|(k, _)| k);
    key
}

/// 取卡片（debug）形状：当前 pending key 是否仍有未上报的预览收敛登记。
/// （暂无调用方需要，保留供将来断言。）
#[allow(dead_code)]
pub fn preview_pending_key() -> Option<String> {
    let g = PREVIEW_PENDING.lock().ok()?;
    g.as_ref().map(|(k, _)| k.clone())
}
