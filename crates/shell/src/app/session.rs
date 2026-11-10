//! 会话临时文件 / 状态恢复：退出时把**主窗口**的选项卡集合落盘到
//! `UserPrefs::session_dir()`（默认 `%APPDATA%\gpui-editor\session\`），下次启动
//! 按清单复原选项卡数量/顺序、未命名内容与活动选项卡。
//!
//! 储存格式：
//! - 清单 `session/session.json`：`{"version":1, "active_tab":N, "tabs":[
//!   {"kind":"file","path":"<绝对路径>"}, {"kind":"new","slot":"t0","name":"未命名"}]}`
//! - 未命名选项卡内容写槽位文件 `session/t0`、`t1`…（LF 规范化，空内容也写空文件）。
//! - 清单 + 槽位一律「临时文件 + rename」原子写；损坏 / 缺字段一律安全回退（不 panic）。
//!
//! **结构**（2026-11 拆 crate）：本模块只保留 host-bound 编排（`impl EditorApp` 的
//! 收集 / 渐进恢复 / 开关 / 退出钩子）；清单 / 槽位 / 导入 / 孤儿清理的**纯存储层**
//! 已迁入独立叶子 crate `session_store`（`crate` 内 `session_store::*`），本模块不再
//! 触及磁盘原语。
//!
//! 范围与限制：
//! - **只持久化/恢复主窗口**（`is_detached == false`）：`new_impl` 仅为主窗口实体
//!   注册 `on_app_quit`，detached 拆分窗口的选项卡不纳入会话。刻意不为多窗口引入
//!   全局协调复杂度（主窗口关闭即视为"应用关闭"）。
//! - **不保存既有文件未落盘的未保存修改**：文件选项卡只记路径，下次启动重新打开
//!   文件本体（需求只要求记住路径；未命名选项卡的内容才进槽位文件）。
//! - 恢复优先级：`new_impl` 中 CLI 首参是文件时保持既有打开行为、跳过恢复（显式
//!   意图优先）；恢复完成后删除清单并清理槽位，避免重复恢复。
//!
//! 开关：`UserPrefs.session_restore`（None = 默认开）经「文件 → 恢复上次会话」菜单
//! 点击切换并立即持久化；关闭时退出不写清单并清理遗留清单/槽位。测试通过显式传入
//! 临时目录的纯逻辑函数（`save_session_to` / `restore_session_from`…）驱动，**从不**
//! 触碰真实 `%APPDATA%` 配置/会话目录。
//!
//! 关闭口径（新）：**点窗口 × 不询问、默认保存**——临时文件由「状态变化即持久化」
//! 连续落盘天然在盘上，下次启动原样恢复；关闭单个**未命名**选项卡才弹确认（是=另存
//! 为、否=删除临时槽位、取消=留在应用），见 `crates/shell/src/app/tabs.rs` 的
//! `close_tab`。每次落盘后 `session_store::prune_unreferenced_slots` 清理清单不再引用
//! 的陈旧槽位，保证「否→删除临时文件」真正落盘、不留孤儿。关窗路径无询问（本平台
//! `App::quit()` 走 `PostQuitMessage`，不触发 `should_close`）。
//!
//! 本工程从未对既有文件正文落盘，关闭单个文件选项卡的「保存/不保存」询问在
//! `tabs.rs::close_tab`（未命名与脏文件），会话目录本身只收纳临时文件。

use super::*;
use session_store::{
    SessionManifest, SessionTab, clear_session, read_manifest, read_slot,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use syntax::{prewarm_done, prewarm_started};

/// 会话持久化防抖窗口（ms）：内容变化后停顿该时长才写盘一次，压缩打字期间的高频重写。
/// 结构变化（新建/关闭/切换选项卡、打开文件、保存）不经防抖，立即落盘。
/// 仅在非测试构建的防抖定时器里被引用（测试构建该分支整体编译掉）→ `#[cfg(not(test))]`
/// 归位，不再退化 `allow(dead_code)`。
#[cfg(not(test))]
const SESSION_FLUSH_DEBOUNCE_MS: u64 = 300;

/// A1/A2：等语法预热的让帧上限（≈1s @60fps）。`prewarm` 实际在启动后台线程早于首帧
/// 完成，正常等待远小于此上限；仅当预热任务异常（不应发生）时兜底，避免恢复挂起。
const RESTORE_PREWARM_MAX_WAIT_FRAMES: u32 = 60;

impl EditorApp {
    /// 「恢复上次会话」开关：翻转 + 立即持久化（不等退出）。关闭时顺带清掉遗留
    /// 会话清单/槽位，避免日后重新开启时恢复陈旧会话。
    pub(super) fn toggle_session_restore(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // SC-3：先提纯 owned 再写回，避免同语句 `borrow_mut`+`borrow`（运行期双借 panic）。
        let session_restore = !self.model.borrow().session_restore;
        self.model.borrow_mut().session_restore = session_restore;
        // 与其它偏好同款落盘（persist_prefs 已纳入 session_restore 字段）。
        self.persist_prefs();
        if !session_restore {
            // 清槽位指纹：重开时若沿用旧指纹会误跳过写槽位（盘面已被清，内容却判「未变」），
            // 导致恢复读到缺失槽位 → 空内容。测试构建不触碰真实会话目录（clear 走显式目录的
            // 纯逻辑），但指纹表在两种构建都清（save_session_to 关闭分支同样兜底）。
            self.model.borrow_mut().session_slot_hashes.clear();
            self.model.borrow_mut().session_last_manifest_digest = None;
            #[cfg(not(test))]
            {
                let _ = clear_session(&UserPrefs::session_dir());
            }
        }
        cx.notify();
    }

    /// 退出钩子入口（`on_app_quit` **兜底**）：把主窗口选项卡保存到 `UserPrefs::session_dir()`。
    /// 开关关闭时只清不写（见 [`save_session_to`]）。常态保存由「状态变化即持久化」
    /// （[`Self::schedule_session_flush`]）承担，本钩子仅在平台确实触发退出回调时做最终落盘。
    /// `#[cfg(not(test))]`：仅 `new_impl` 的 `#[cfg(not(test))]` 退出钩子调用，测试构建下
    /// 无此函数（测试直接驱动 `save_session_to` 纯逻辑），归位后无需 `allow(dead_code)`。
    #[cfg(not(test))]
    pub(super) fn save_session(&mut self, cx: &Context<Self>) {
        let _ = save_session_to(self, &UserPrefs::session_dir(), cx);
    }

    /// 启动恢复入口（`new_impl` 里无 CLI 首参时调用）：开关开启且清单存在且有效才恢复。
    /// **A1 非阻塞化**：首帧前只「读清单 + 登记首帧后逐步恢复」（不打开任何选项卡）；
    /// 逐选项卡打开从首帧前同步段挪到首帧 render 之后按帧逐个进行（窗口先渲染、tab
    /// 逐个渐进出现），最终态（顺序 / 活动 / dirty / 内容）与改前同步恢复完全一致
    /// （见 [`Self::schedule_restore`]／`restore_step`；同步版 [`restore_session_from`]
    /// 保留给测试/基准/导入设置走显式目录）。
    /// `#[cfg(not(test))]`：见 [`Self::save_session`]。
    #[cfg(not(test))]
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = UserPrefs::session_dir();
        if !self.model.borrow().session_restore {
            return;
        }
        let Some(manifest) = read_manifest(&dir) else {
            return;
        };
        self.schedule_restore(window, cx, dir, manifest);
    }

    /// A1：登记「首帧后按帧逐个恢复」的恢复通行证。`manifest` 已由调用方读取
    /// （[`Self::restore_session`] 内部读真实会话目录；测试直读显式临时目录）。
    /// 此处只做：置恢复期间抑制持久化 → 把待恢复选项卡排队 → 登记下一帧回调。
    /// **本函数内不打开任何选项卡**——首个真实 `open_file` 发生在首帧 render 之后
    /// （`on_next_frame` 保证），因此首帧不含恢复段。
    /// `#[doc(hidden)] pub`：启动（`restore_session`）与测试（显式目录 +
    /// `simulate_next_frame`）共用；integration 目标经 pub 面访问（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn schedule_restore(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        dir: PathBuf,
        manifest: SessionManifest,
    ) {
        // 恢复期间抑制持久化（同 [`restore_session_from`] 语义，覆盖整个渐进恢复窗口）
        self.model.borrow_mut().session_restoring = true;
        let SessionManifest { active_tab, tabs, .. } = manifest;
        let pending = VecDeque::from(tabs);
        cx.on_next_frame(window, move |this, window, cx| {
            this.restore_step(window, cx, dir, active_tab, pending, 0);
        });
    }

    /// A1 渐进恢复单步：开下一个选项卡并登记下一帧回调；全部完成后统一收尾（活动
    /// 选项卡 clamp、放行持久化、删除清单/槽位、状态栏、notify）。文件选项卡走既有
    /// `open_file`（小文档同步挂载、大文档走 `spawn_progressive_open` 后台渐进）——
    /// 每开一个之间让出一帧，窗口保持响应、tab 逐个渐进出现，最终态与同步恢复一致
    /// （K35：源活动下标与各 tab 内容/路径/dirty 不变由测试断言）。
    ///
    /// A2 语法预热竞态：打开文件选项卡前，若预热已启动但未完成（`prewarm_started &&
    /// !prewarm_done`），让帧等待（等上限 [`RESTORE_PREWARM_MAX_WAIT_FRAMES`] 帧），
    /// 保证 `set_language → find_syntax` 全部命中缓存、主线程零冷扫；上限内仍没完成
    /// （预热任务异常，不应发生）→ 照开，退化到改前的冷扫行为，不挂起。
    fn restore_step(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        dir: PathBuf,
        active_tab: usize,
        mut pending: VecDeque<SessionTab>,
        prewarm_wait: u32,
    ) {
        if let Some(next) = pending.pop_front() {
            match next {
                SessionTab::File { path } => {
                    if path.is_file() {
                        if prewarm_started()
                            && !prewarm_done()
                            && prewarm_wait < RESTORE_PREWARM_MAX_WAIT_FRAMES
                        {
                            pending.push_front(SessionTab::File { path });
                            cx.on_next_frame(window, move |this, window, cx| {
                                this.restore_step(
                                    window,
                                    cx,
                                    dir,
                                    active_tab,
                                    pending,
                                    prewarm_wait + 1,
                                );
                            });
                            return;
                        }
                        self.open_file(&path.to_string_lossy(), window, cx);
                    }
                }
                SessionTab::New { slot, .. } => {
                    let idx = self.make_tab(window, cx);
                    let text = read_slot(&dir, &slot).unwrap_or_default();
                    self.set_tab_text(idx, text, window, cx);
                }
            }
            cx.on_next_frame(window, move |this, window, cx| {
                this.restore_step(window, cx, dir, active_tab, pending, 0);
            });
        } else {
            // 全部选项卡已恢复：活动选项卡越界收敛到合法值（多开/缺文件只多不少时同 clamp）。
            // SC-2：先提纯 len（owned）再写，避免同一语句 `borrow_mut()` 与 `borrow()`
            // 同时存活（运行期 RefCell 双借 panic）。
            let len = self.model.borrow().tabs.len();
            self.model.borrow_mut().active_tab = if len == 0 {
                0
            } else {
                active_tab.min(len - 1)
            };
            // 恢复完成放行持久化，随后删除清单/槽位（避免重复恢复）。
            self.model.borrow_mut().session_restoring = false;
            clear_session(&dir);
            // 盘面已被清（槽位/清单删除）：指纹表也必须清，否则此后首次持久化会拿
            //「恢复前内容」的旧指纹误判「未变」而跳过写槽位/清单 → 恢复出的选项卡内容不再落盘。
            self.model.borrow_mut().session_slot_hashes.clear();
            self.model.borrow_mut().session_last_manifest_digest = None;
            // D1（评审修复）：恢复窗口内结构性变更（新建未命名 tab / 打开文件）的
            // `schedule_session_flush(immediate)` 被 `session_restoring` 抑制，且结构变更
            // 不走防抖重排 → 补一发**即时补偿落盘**（此刻 `session_restoring` 已置 false、
            // 指纹表已清，`persist_session` 会写全新指纹，避免误跳过槽位/清单），把恢复
            // 出的选项卡集合写回会话目录作新基线——退出/崩溃时未命名 tab 内容不丢。
            self.schedule_session_flush(cx, true);
            self.model.borrow_mut().status = SharedString::from("已恢复上次会话");
            self.model.borrow_mut().status_ok = true;
            // A1 测量：渐进恢复完成墙钟（发生在首帧之后，[STARTUP] 汇总行打印在前，
            // 故此处按 boot 基准补打一行，供 before/after 对比「恢复总耗时」）。
            if let Some(ms) = base::startup::elapsed_ms() {
                eprintln!("[STARTUP] progressive_restore_done={ms:.1}ms");
            }
            cx.notify();
        }
    }

    /// 把当前**主窗口**选项卡集合**立即**持久化到会话目录（连续持久化核心：关闭程序时
    /// 状态本就在盘上，天然覆盖全部退出路径——退出钩子在部分平台不触发）。
    /// 仅在**会话责任窗口**（首个主窗口 `session_persist`）、非 detached、开关开启、
    /// 非恢复期间落盘；测试构建恒跳过（**绝不**触碰真实会话目录，验证走显式临时目录的
    /// 纯逻辑 [`AppModel::save_session_to`]，见 `tests/session.rs`）。
    /// SC-4：编排逻辑已迁 `AppModel::persist_session`（shell_core::actions）；此处 glue 转发。
    pub(super) fn persist_session(&mut self, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().persist_session(host, &mut **cx);
    }

    /// 安排一次会话持久化：结构变化 `immediate=true` 立即写盘；内容变化 `false` 走
    /// [`SESSION_FLUSH_DEBOUNCE_MS`] 防抖（打字停顿后才写一次，不逐键写盘）。收敛自
    /// 侧边栏搜索防抖的时序代号模式（`base::SeqAfter` 可重入、代序作废）：防抖期间又
    /// 被安排 → 新代号覆盖，最终只落盘一次最新状态。超时 / 唤醒失败静默丢弃，不 panic。
    pub(super) fn schedule_session_flush(&mut self, cx: &mut Context<Self>, immediate: bool) {
        if immediate {
            self.persist_session(cx);
            return;
        }
        // 防抖只挂在非测试构建：测试直接驱动纯逻辑函数验证，不在测试进程残留定时任务。
        #[cfg(not(test))]
        {
            let weak = cx.weak_entity();
            self.model.borrow().session_flush_debounce.after(
                cx,
                std::time::Duration::from_millis(SESSION_FLUSH_DEBOUNCE_MS),
                move |app| {
                    let _ = weak.update(app, |this, cx| {
                        this.persist_session(cx);
                    });
                },
            );
        }
    }
}

/// 从当前选项卡集合生成会话数据（清单 + 各未命名槽位内容，纯内存、无磁盘副作用）。
/// 与「导出设置」（`settings_xfer`）共用同一序列化口径：文件选项卡只记路径、未命名
/// 选项卡经 [`AppModel::tab_effective_text`] 取有效内容（live2/WYSIWYG 开启时读 WyEditor
/// 实时内容）。槽位名 `t0..tN` 随未命名顺序位移。
/// SC-4：逻辑已迁 `AppModel::collect_session_data`；此处保留 free fn 挂导出设置/测试。
pub(crate) fn collect_session_data(
    app: &EditorApp,
    cx: &App,
) -> (SessionManifest, Vec<(String, String)>) {
    app.model.borrow().collect_session_data(cx)
}

/// 保存会话（纯逻辑，显式目录，可测试）：SC-4 逻辑已迁
/// [`AppModel::save_session_to`]（shell_core::actions）；此处保留 free fn 挂测试与
/// 退出钩子（`save_session`），逐字保持既有语义（开关关闭只清、未变槽位跳过等）。
/// `#[doc(hidden)] pub`：integration 测试经 `shell::app::save_session_to` 访问（§8.6）。
#[doc(hidden)]
pub fn save_session_to(app: &mut EditorApp, dir: &Path, cx: &App) -> bool {
    app.model.borrow_mut().save_session_to(dir, cx)
}

/// 恢复会话（纯逻辑，显式目录，**同步版**）：读清单 → 依次开文件 / 建未命名选项卡并
/// 灌槽位内容 → 活动选项卡按清单取并 clamp 到 `[0, tabs.len())` → 删除清单并清理
/// 槽位（避免重复恢复）。开关关闭 / 清单缺失 → false（不动作）。文件已不存在 → 跳过
/// （与 CLI 首参打开同口径：`is_file` 才开）。
/// **A1 之后生产启动不再走本函数**（改走渐进的 [`EditorApp::schedule_restore`]）；此处
/// 保留为测试与基准（`tests/session.rs`、`tests/bench.rs`、`tests/settings_xfer.rs`）
/// 的「改前行为基准」驱动。原 `#[cfg(test)]` 归位为恒编：integration 目标下 shell lib
/// 以非 test 编译，`#[cfg(test)]` 项不可见，必须恒编 + `pub`（生产不调用，pub 故无
/// dead_code 告警）。
#[doc(hidden)]
pub fn restore_session_from(
    app: &mut EditorApp,
    dir: &Path,
    window: &mut Window,
    cx: &mut Context<EditorApp>,
) -> bool {
    if !app.model.borrow().session_restore {
        return false;
    }
    let Some(manifest) = read_manifest(dir) else {
        return false;
    };
    // 恢复期间抑制持久化：重建选项卡 / 灌内容不触发写盘（避免反复落盘 / 抵抗尾部
    // `clear_session` 清理）。
    app.model.borrow_mut().session_restoring = true;
    for tab in &manifest.tabs {
        match tab {
            SessionTab::File { path } => {
                if path.is_file() {
                    app.open_file(&path.to_string_lossy(), window, cx);
                }
            }
            SessionTab::New { slot, .. } => {
                let idx = app.make_tab(window, cx);
                let text = read_slot(dir, slot).unwrap_or_default();
                app.set_tab_text(idx, text, window, cx);
            }
        }
    }
    // 活动选项卡越界收敛到合法值（多开/缺文件只多不少时同样 clamp）。
    // SC-2：先提纯 len（owned）再写，避免同一语句 `borrow_mut()` 与 `borrow()`
    // 同时存活（运行期 RefCell 双借 panic）。
    let len = app.model.borrow().tabs.len();
    app.model.borrow_mut().active_tab = if len == 0 {
        0
    } else {
        manifest.active_tab.min(len - 1)
    };
    // 恢复完成放行持久化，随后既有 `clear_session` 删除清单/槽位（避免重复恢复）。
    app.model.borrow_mut().session_restoring = false;
    clear_session(dir);
    // 盘面已被清（槽位/清单删除）：指纹表也必须清，否则此后首次持久化会拿「恢复前内容」
    // 的旧指纹误判「未变」而跳过写槽位/清单 → 恢复出的选项卡内容不再落盘。
    app.model.borrow_mut().session_slot_hashes.clear();
    app.model.borrow_mut().session_last_manifest_digest = None;
    app.model.borrow_mut().status = SharedString::from("已恢复上次会话");
    app.model.borrow_mut().status_ok = true;
    cx.notify();
    true
}
