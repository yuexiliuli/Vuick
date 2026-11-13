//! 真机 Tab 键路由/渲染诊断探针（追查「真机按 Tab 光标动但没插入/无效果」的证据通道，
//! K46：headless 全绿 ≠ 真机行为——`#[gpui::test]` 用 `NoopTextSystem`（每字符统一
//! 0.6em 宽），生产 Windows 用 `DirectWriteTextSystem`，两类整形语义可能不同）。
//!
//! - 默认关闭：`GPUI_EDITOR_TAB_DIAG=1` 才启用；进程级 `OnceLock` 一次判读，未开时
//!   每次调用仅一次原子装载即返回，**零分配、零 IO**（detail 经闭包懒构造）。
//! - 只在**事件点**调用（Root `on_action_tab` / 编辑区 `tab()` / WY `tab_insert`），
//!   不逐帧刷屏。
//! - 追加写 `%APPDATA%\gpui-editor\gpui-editor-tab.log`（测试构建写 `%TEMP%\gpui-editor-tab.log`，
//!   不污染真实 APPDATA）。写失败静默丢弃，绝不影响主流程。
//! - 结束运行**不清理**，保留供主 agent 读取；`.gitignore` 收 `gpui-editor-tab.log` 不入库。

use std::path::PathBuf;
use std::sync::OnceLock;

/// 探针是否开启（进程级缓存一次判读）：`GPUI_EDITOR_TAB_DIAG=1`；其余值 / 缺失 → 关。
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("GPUI_EDITOR_TAB_DIAG").as_deref(), Ok("1")))
}

/// 测试构建判定（与 `shell_core::rp_diag` / `editor::diag` 同口径：`cfg!(test)` 或 env 注入）。
fn is_test_build() -> bool {
    cfg!(test) || matches!(std::env::var("GPUI_EDITOR_TEST").as_deref(), Ok("1"))
}

/// 日志文件路径：测试构建 → `%TEMP%\gpui-editor-tab.log`；生产 → `%APPDATA%\gpui-editor\gpui-editor-tab.log`。
fn log_path() -> PathBuf {
    if is_test_build() {
        return std::env::temp_dir().join("gpui-editor-tab.log");
    }
    match std::env::var_os("APPDATA").map(PathBuf::from) {
        Some(base) => base.join("gpui-editor").join("gpui-editor-tab.log"),
        _ => PathBuf::from("gpui-editor-tab.log"),
    }
}

/// 追加一行 `[TAB] <key> <detail>`。**仅在 `GPUI_EDITOR_TAB_DIAG=1` 时**分配 + 写盘；
/// 未开启时 = 一次 `OnceLock` 原子装载（detail 闭包不构造，零分配零 IO）。
#[inline]
pub fn diag(key: &str, detail: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    let line = format!("[TAB] {key} {}\n", detail());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = std::io::Write::write_all(&mut f, line.as_bytes());
    }
}
