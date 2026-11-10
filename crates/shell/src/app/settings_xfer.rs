//! 「导出设置… / 导入设置…」（文件菜单入口，系统保存/打开对话框）：配置 + 会话
//! **全量**打包为单个 `.json`，自含魔力头与版本可校验——「搬到别处导入也还在」。
//!
//! 打包格式（单个 `.json`）：
//! ```json
//! { "app": "gpui-editor", "export_version": 1,
//!   "prefs": { ...UserPrefs 全字段... },
//!   "session": { "manifest": { "version":1, "active_tab":N, "tabs":[...] },
//!                "slots": { "t0": "<正文>", ... } } }
//! ```
//! - prefs 复用 `UserPrefs::to_json_value`（[`EditorApp::collect_prefs`] 与持久化同一口径，
//!   导出即「当前交互态」）；
//! - session 复用 [`collect_session_data`]（与 [`crate::app::session::save_session_to`]
//!   同一序列化路径，不复制整套会话逻辑），各槽位正文经 serde_json 字符串转义嵌入。
//!
//! **结构**（2026-11 拆 crate）：打包包的**纯序列化**（构建 / 校验 / 解析）已迁入独立
//! 叶子 crate `session_store`（`session_store::build_export_bundle` /
//! `parse_export_bundle`）；本模块只保留 host-bound 应用层（对话框 / 配置即时生效 /
//! 会话写回目录 / toast）。
//!
//! 导入语义：配置**即时生效**（主题/字号/侧栏宽度立即刷新并持久化本地 config，下一次
//! 启动仍保持）；会话写回当前 session 目录、**下一启动** [`restore_session_from`] 恢复
//! （不立即重排当前窗口，避免打断未保存编辑）。部分可解析：prefs 有效但 session 损坏 →
//! 配置照常应用 + toast 说明跳过；反向同理，不整体回滚。损坏/版本不符/非本应用文件 →
//! toast「不是有效的 gpui-editor 设置文件」，零副作用零误应用。

use super::*;
use super::session::{collect_session_data};
use session_store::{
    ParsedExportBundle, build_export_bundle, clear_session,
    write_imported_session,
};
// `parse_export_bundle` 仅 `import_settings`（`#[cfg(not(test))]` 对话框入口）消费；
// 测试构建无该调用 → 此处按构建门控 import，避免测试构建未用告警。
#[cfg(not(test))]
use session_store::parse_export_bundle;
use ui::notification::{Notification, NotificationKind};

/// 「导出/导入设置」结果通知的（伪）类型标签：配合唯一 key 精确移除（同
/// `goto_line_toast` 姿势，替换上一条同款提示不堆积）。
struct SettingsTransferToast;

/// 本次导入中会话段的处置结果（决定 toast 文案，区分「无会话 / 损坏 / 已写入 /
/// 按导入值显式关闭 / 写失败」）。
enum SessionOutcome {
    /// 导出包无 session 段。
    Absent,
    /// session 段存在但损坏（清单缺失 / 非对象）。
    Invalid,
    /// 已写回会话目录（下一启动恢复）。
    Written,
    /// 会话有效但「恢复上次会话」按导入值为显式关闭 → 诚实跟随：未写入（已清）。
    SkippedRestoreOff,
    /// 写入失败。
    WriteFailed,
}

impl EditorApp {
    /// 「导出设置…」（文件菜单）：系统保存对话框（默认名 `gpui-editor-设置-YYYYMMDD.json`、
    /// `.json` 过滤）→ 从**当前交互态**生成导出包 → 原子写盘 → toast 结果。
    /// 测试构建不真弹原生对话框（同 `open_dialog` 姿势）；取消 → 静默不动作。
    pub(super) fn export_settings(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let window = _window;
            let cx = _cx;
            let entity = cx.entity();
            let default_name = format!("gpui-editor-设置-{}.json", local_yyyymmdd());
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("导出设置")
                        .set_file_name(&default_name)
                        .add_filter("JSON", &["json"])
                        .save_file()
                        .await
                    else {
                        return; // 取消：静默（对齐既有对话框取消语义）
                    };
                    let save_path = file.path().to_path_buf();
                    let _ = cx.update_window_entity(&entity, |this, _window, cx| {
                        this.export_to_path(&save_path, cx);
                    });
                })
                .detach();
        }
    }

    /// 把当前应用状态导出到指定路径（纯逻辑、可测）：收集 prefs（当前交互态）+ 会话
    /// 清单/槽位 → 构建导出包 → 原子写盘 → toast（成功带路径 / 失败带原因，K11 不崩）。
    /// `#[doc(hidden)] pub`：`tests/session.rs`（settings_xfer 合并域）直驱（§8.6）。
    #[doc(hidden)]
    pub fn export_to_path(&mut self, save_path: &std::path::Path, cx: &mut Context<Self>) {
        let prefs = self.collect_prefs().to_json_value();
        let (manifest, slots) = collect_session_data(self, cx);
        let body = build_export_bundle(&prefs, &manifest, &slots);
        match base::util::atomic_write(save_path, body.as_bytes()) {
            Ok(()) => self.settings_toast(format!("已导出设置到 {}", save_path.display()), cx),
            Err(e) => self.settings_toast(format!("导出设置失败：{e}"), cx),
        }
    }

    /// 「导入设置…」（文件菜单）：系统打开对话框（`.json` 过滤）→ 读取 + 校验导出包 →
    /// 应用（配置即时生效 / 会话写回 session 目录）→ toast 结果。
    /// 测试构建不真弹原生对话框；取消 → 静默不动作。
    pub(super) fn import_settings(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let window = _window;
            let cx = _cx;
            let entity = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("导入设置")
                        .add_filter("JSON", &["json"])
                        .pick_file()
                        .await
                    else {
                        return; // 取消：静默（对齐既有对话框取消语义）
                    };
                    let path = file.path().to_path_buf();
                    let text = std::fs::read_to_string(&path).ok();
                    let parsed = text.as_deref().and_then(|t| parse_export_bundle(t).ok());
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        let session_dir = config::user::UserPrefs::session_dir();
                        this.apply_import_bundle(parsed, &session_dir, window, cx);
                    });
                })
                .detach();
        }
    }

    /// 把一次导入应用到当前会话（纯逻辑、可测，测试直接驱动显式 session 目录）：
    /// - prefs → 配置即时生效（主题/字号/侧栏宽度立即刷新）+ 持久化本地 config
    ///   （下一次启动仍保持）；`session_restore` 显式 false → 诚实跟随（清 session 目录）。
    /// - session → 写回 `session_dir`（下一启动恢复）；**不**立即重排当前窗口。
    /// 部分可解析：单侧损坏只跳过该侧 + toast 说明，不整体回滚；整体非法 → 零副作用。
    /// `#[doc(hidden)] pub`：`tests/session.rs`（settings_xfer 合并域）直驱（§8.6）。
    #[doc(hidden)]
    pub fn apply_import_bundle(
        &mut self,
        parsed: Option<ParsedExportBundle>,
        session_dir: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bundle) = parsed else {
            // 非本应用 / 损坏 / 版本不符：toast，不应用任何东西（零副作用零误应用）。
            self.settings_toast("不是有效的 gpui-editor 设置文件".to_string(), cx);
            return;
        };
        let mut ok_prefs = false;
        if let Some(pv) = bundle.prefs {
            let prefs = UserPrefs::from_json_value(&pv);
            // 主题即时生效：偏好三态落地 + 刷新全局主题与各预览/画布背景。
            self.model.borrow_mut().theme_pref = prefs.theme;
            self.model.borrow_mut().dark_mode = prefs.theme.resolved_dark(cx.window_appearance());
            // PDF 画布背景偏好：**先于** refresh_theme_ui 落地（其 default 守卫按新值走），
            // 之后再按偏好应用显式色到各 PDF 画布（default 档由守卫回落主题）。
            self.model.borrow_mut().pdf_bg = prefs.pdf_bg;
            self.model.borrow_mut().pdf_bg_custom = prefs.pdf_bg_custom.clone();
            self.refresh_theme_ui(cx);
            self.apply_pdf_backgrounds(cx);
            // 字号即时生效（沿用启动同款 clamp / 默认口径）。
            self.model.borrow_mut().font_size = prefs
                .font_size
                .map(|v| v.clamp(FONT_MIN, FONT_MAX))
                .unwrap_or(DEFAULT_FONT_SIZE);
            // 侧栏宽度即时生效（None = 恢复随内容自适应）。
            self.sidebar.width = prefs.sidebar_width.unwrap_or(SIDEBAR_WIDTH);
            self.sidebar.width_auto = prefs.sidebar_width.is_none();
            // 会话恢复 / 关闭到托盘开关：以导入值为准（诚实跟随，含显式 false）。
            self.model.borrow_mut().session_restore = prefs.session_restore.unwrap_or(true);
            self.model.borrow_mut().close_to_tray = prefs.close_to_tray.unwrap_or(false);
            // 媒体打开位置两个开关：以导入值为准（诚实跟随，含显式 false），即时生效。
            self.model.borrow_mut().media_new_window = prefs.media_new_window.unwrap_or(true);
            self.model.borrow_mut().reuse_media_window = prefs.reuse_media_window.unwrap_or(true);
            // 媒体窗口记录尺寸：以导入值为准（None = 还原为「新开跟随发起窗口大小」）。
            self.model.borrow_mut().music_window_size = prefs
                .music_window_size
                .map(|(w, h)| gpui::size(px(w), px(h)));
            self.model.borrow_mut().video_window_size = prefs
                .video_window_size
                .map(|(w, h)| gpui::size(px(w), px(h)));
            // 最近打开列表随配置恢复。
            self.model.borrow_mut().recent_folders = prefs.recent_folders;
            self.model.borrow_mut().recent_files = prefs.recent_files;
            // 打开文件夹根：有效目录 → 设根并（与启动/换根同一语义）后台扫根直接子项
            // ——只设根不拉侧栏，也绝不把树停在「根已展开却永不填充」的死态。路径失效/
            // 无效 → 不设根（首次 persist 时按既有路径写回 null）。
            if let Some(folder) = prefs.opened_folder {
                let p = PathBuf::from(&folder);
                if p.is_dir() {
                    self.sidebar.files.set_root(Some(p.clone()));
                    self.sidebar.files.expanded.insert(p.clone());
                    self.spawn_file_tree_scan(p, window, cx);
                }
            }
            // 持久化到本地 config（下一次启动仍保持）。
            self.persist_prefs();
            ok_prefs = true;
        }
        // 会话：写回当前 session 目录、下一启动恢复；开关按导入值显式关闭 → 诚实跟随
        // （清而不写，对齐既有「开关关只清不写」语义）。
        let out = if let Some((manifest, slots)) = bundle.session {
            if self.model.borrow().session_restore {
                if write_imported_session(session_dir, &manifest, &slots) {
                    SessionOutcome::Written
                } else {
                    SessionOutcome::WriteFailed
                }
            } else {
                let _ = clear_session(session_dir);
                SessionOutcome::SkippedRestoreOff
            }
        } else if bundle.session_present {
            SessionOutcome::Invalid
        } else {
            SessionOutcome::Absent
        };
        cx.notify();
        // 组合结果文案（部分可解析：无整体回滚；区分「无会话 / 损坏 / 恢复开关关闭」）。
        let msg = if ok_prefs {
            match out {
                SessionOutcome::Written => "已导入设置（配置 + 会话；会话下一启动恢复）".to_string(),
                SessionOutcome::Absent => "配置已导入（导出包无会话）".to_string(),
                SessionOutcome::Invalid => "配置已导入，会话无效已跳过".to_string(),
                SessionOutcome::SkippedRestoreOff => {
                    "配置已导入（恢复上次会话已关闭，未写入会话）".to_string()
                }
                SessionOutcome::WriteFailed => "配置已导入，会话写入失败".to_string(),
            }
        } else if matches!(out, SessionOutcome::Written) {
            "配置无效已跳过，会话已导入".to_string()
        } else {
            "没有可导入的有效内容".to_string()
        };
        self.settings_toast(msg, cx);
    }

    /// 右上角 toast（导出/导入结果统一入口）：替换上一条同款提示（自持 key 不堆积，
    /// 同 `goto_line_toast` 姿势）。
    fn settings_toast(&mut self, msg: String, cx: &mut Context<Self>) {
        let key = self.model.borrow_mut().settings_toast_key.take();
        if let Some(key) = key {
            self.remove_notification1::<SettingsTransferToast>(key);
        }
        let key = gpui::ElementId::from(SharedString::from(format!(
            "settings-xfer-toast-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )));
        self.model.borrow_mut().settings_toast_key = Some(key.clone());
        self.push_notification(
            Notification::new()
                .id1::<SettingsTransferToast>(key)
                .title("设置导出/导入")
                .message(msg)
                .with_type(NotificationKind::Error),
        );
        cx.notify();
    }
}

/// 本地日期 `YYYYMMDD`（仅导出对话框默认文件名；无日期库依赖，纯算术）。
/// 注意：按 UTC 换算日序（与系统时区可能差一天），文件名仅作默认建议值，不影响内容。
#[cfg(not(test))]
fn local_yyyymmdd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 民用历换算，无依赖）。
    let days = (secs / 86_400) as i64 + 719_468;
    let era = (if days >= 0 { days } else { days - 146_096 }) / 146_097;
    let doe = days - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}{m:02}{d:02}")
}
