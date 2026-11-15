//! 编辑器滚动诊断（真机「md 重开：编辑区 47→2 被拉回顶端」追查：找出每个 editor 滚动
//! 的确切来源）。
//!
//! 默认关（`GPUI_EDITOR_RP_DIAG=1` 开启，开关缓存一次判读）。为**同一份**
//! `rp_diag` 日志追加 `[RP] ed_scroll from=<site> …` 行——开关/测试判定/日志路径收敛自
//! `base::diag`（`env_flag` + `diag_log_path`，本批 Pattern 7）：测试构建写
//! `%TEMP%\gpui-rp-diag.log`、生产写 `%APPDATA%\gpui-editor\rp_diag.log`（与
//! `shell_core::rp_diag` 同一位置、行可交错归因；旧镜像里 shell_core 侧曾随
//! `config_path` 覆盖改变位置，收敛后统一 APPDATA 口径）。
//! 只在**实际发起滚动**的点调用（数值经 `scroll_to_px` 单一咽喉/`desired_scroll` 直写），
//! 不逐帧刷屏；探针末行命中即输出，跨文件归因。

/// 探针是否开启（进程级缓存一次判读）：`GPUI_EDITOR_RP_DIAG=1`；其余值 / 缺失 → 关。
/// 开关/测试判定/路径收敛自 `base::diag`（本批 Pattern 7）。
pub fn enabled() -> bool {
    base::diag::env_flag("GPUI_EDITOR_RP_DIAG")
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
        .open(base::diag::diag_log_path("rp_diag.log"))
    {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
}
