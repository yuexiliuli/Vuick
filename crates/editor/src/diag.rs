//! 编辑器滚动诊断（真机「md 重开：编辑区 47→2 被拉回顶端」追查：找出每个 editor 滚动
//! 的确切来源）。
//!
//! 默认关（`GPUI_EDITOR_RP_DIAG=1` 开启，进程级 OnceLock 一次判读）。为**同一份**
//! `rp_diag` 日志追加 `[RP] ed_scroll from=<site> …` 行——本 crate 不能依赖 shell_core
//! （shell_core → editor 反向即为环），故开关/路径与 `shell_core::rp_diag` 镜像：
//! 测试构建写 `%TEMP%\gpui-rp-diag.log`、生产写 `%APPDATA%\gpui-editor\rp_diag.log`。
//! 只在**实际发起滚动**的点调用（数值经 `scroll_to_px` 单一咽喉/`desired_scroll` 直写），
//! 不逐帧刷屏；探针末行命中即输出，跨文件归因。

use std::path::PathBuf;
use std::sync::OnceLock;

/// 探针是否开启（进程级缓存一次判读）：`GPUI_EDITOR_RP_DIAG=1`；其余值 / 缺失 → 关。
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("GPUI_EDITOR_RP_DIAG").as_deref(), Ok("1")))
}

/// 测试构建判定（与 `shell_core::rp_diag` 同口径：`cfg!(test)` 或 env 注入）。
fn is_test_build() -> bool {
    cfg!(test) || matches!(std::env::var("GPUI_EDITOR_TEST").as_deref(), Ok("1"))
}

/// 日志文件路径：与 `shell_core::rp_diag` 完全一致（同一份日志，行可交错归因）。
fn log_path() -> PathBuf {
    if is_test_build() {
        return std::env::temp_dir().join("gpui-rp-diag.log");
    }
    match std::env::var_os("APPDATA").map(PathBuf::from) {
        Some(base) => base.join("gpui-editor").join("rp_diag.log"),
        _ => PathBuf::from("rp_diag.log"),
    }
}

/// 追加一行 `[RP] ed_scroll from=<site> <detail>`（追加、UTF-8；写失败静默丢弃，
/// 绝不影响主流程）。
pub fn ed_scroll(site: &str, detail: &str) {
    if !enabled() {
        return;
    }
    let line = format!("[RP] ed_scroll from={site} {detail}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
}
