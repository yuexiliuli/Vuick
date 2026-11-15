//! 真机 Tab 键路由/渲染诊断探针（追查「真机按 Tab 光标动但没插入/无效果」的证据通道，
//! K46：headless 全绿 ≠ 真机行为——`#[gpui::test]` 用 `NoopTextSystem`（每字符统一
//! 0.6em 宽），生产 Windows 用 `DirectWriteTextSystem`，两类整形语义可能不同）。
//!
//! - 默认关闭：`GPUI_EDITOR_TAB_DIAG=1` 才启用；开关/测试判定/日志路径收敛到
//!   [`crate::diag`]（本批 Pattern 7），未开时每次调用仅一次缓存查即返回，**零分配、零 IO**。
//! - 只在**事件点**调用（Root `on_action_tab` / 编辑区 `tab()` / WY `tab_insert`），
//!   不逐帧刷屏。
//! - 追加写 `%APPDATA%\gpui-editor\gpui-editor-tab.log`（测试构建写 `%TEMP%\gpui-editor-tab.log`，
//!   不污染真实 APPDATA）。写失败静默丢弃，绝不影响主流程。
//! - 结束运行**不清理**，保留供主 agent 读取；`.gitignore` 收 `gpui-editor-tab.log` 不入库。

/// 追加一行 `[TAB] <key> <detail>`。**仅在 `GPUI_EDITOR_TAB_DIAG=1` 时**分配 + 写盘；
/// 未开启时 = 一次缓存查即返回（detail 闭包不构造，零分配零 IO）。
/// 开关/路径收敛自 [`crate::diag`]（`env_flag` + `diag_log_path`，本批 Pattern 7）。
#[inline]
pub fn diag(key: &str, detail: impl FnOnce() -> String) {
    if !crate::diag::env_flag("GPUI_EDITOR_TAB_DIAG") {
        return;
    }
    let line = format!("[TAB] {key} {}\n", detail());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::diag::diag_log_path("gpui-editor-tab.log"))
    {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
}
