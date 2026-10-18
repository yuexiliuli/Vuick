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
//! `close_tab`。每次落盘后 [`prune_unreferenced_slots`] 清理清单不再引用的陈旧槽位，
//! 保证「否→删除临时文件」真正落盘、不留孤儿。关窗路径无询问（本平台
//! `App::quit()` 走 `PostQuitMessage`，不触发 `should_close`）。
//!
//! 本工程从未对既有文件正文落盘，关闭单个文件选项卡的「保存/不保存」询问在
//! `tabs.rs::close_tab`（未命名与脏文件），会话目录本身只收纳临时文件。

use super::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 会话持久化防抖窗口（ms）：内容变化后停顿该时长才写盘一次，压缩打字期间的高频重写。
/// 结构变化（新建/关闭/切换选项卡、打开文件、保存）不经防抖，立即落盘。
/// 仅在非测试构建的防抖定时器里被引用（测试构建该分支整体编译掉）→ `#[cfg(not(test))]`
/// 归位，不再退化 `allow(dead_code)`。
#[cfg(not(test))]
const SESSION_FLUSH_DEBOUNCE_MS: u64 = 300;

/// 清单里单个会话选项卡。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionTab {
    /// 既有文件：下次启动按路径重新打开。
    File { path: PathBuf },
    /// 未命名选项卡：内容存槽位文件（`session/t0`…）；`name` 仅作清单可读性保留
    /// （EditorTab 无名字段，恢复后仍显示常量「未命名」）。
    New { slot: String, name: String },
}

/// 会话清单。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct SessionManifest {
    /// 格式版本（当前 1；未知版本一律回退默认，不 panic）。
    pub version: u32,
    /// 活动选项卡下标（恢复时 clamp 到合法范围）。
    pub active_tab: usize,
    /// 按顺序的选项卡列表（顺序即恢复顺序）。
    pub tabs: Vec<SessionTab>,
}

impl SessionTab {
    fn to_value(&self) -> Value {
        match self {
            SessionTab::File { path } => json!({ "kind": "file", "path": path.to_string_lossy() }),
            SessionTab::New { slot, name } => json!({ "kind": "new", "slot": slot, "name": name }),
        }
    }

    /// 解析选项卡元素：字段缺 / 类型不符 → None（安全回退整项）。
    fn from_value(v: &Value) -> Option<SessionTab> {
        let obj = v.as_object()?;
        match obj.get("kind")?.as_str()? {
            "file" => {
                let path = PathBuf::from(obj.get("path")?.as_str()?);
                Some(SessionTab::File { path })
            }
            "new" => Some(SessionTab::New {
                slot: obj.get("slot")?.as_str()?.to_string(),
                name: obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("未命名")
                    .to_string(),
            }),
            _ => None,
        }
    }
}

impl SessionManifest {
    fn to_value(&self) -> Value {
        json!({
            "version": self.version,
            "active_tab": self.active_tab,
            "tabs": self.tabs.iter().map(SessionTab::to_value).collect::<Vec<_>>(),
        })
    }

    /// 清单回退语义：非对象 / 缺字段 / 损坏 → 最小可用清单（不 panic）。
    fn from_value(v: &Value) -> SessionManifest {
        let Some(obj) = v.as_object() else {
            return Self::default();
        };
        let version = obj
            .get("version")
            .and_then(|x| x.as_u64())
            .unwrap_or(1)
            .min(u32::MAX as u64) as u32;
        let active_tab = obj.get("active_tab").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        let tabs = obj
            .get("tabs")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(SessionTab::from_value).collect())
            .unwrap_or_default();
        SessionManifest {
            version,
            active_tab,
            tabs,
        }
    }
}

/// 清单文件路径。
fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

/// 槽位文件路径（`t0`、`t1`…）。
fn slot_path(dir: &Path, slot: &str) -> PathBuf {
    dir.join(slot)
}

/// 「临时文件 + rename」原子写（Windows rename 不覆盖 → 先删旧文件，仿 config/user.rs）。
fn atomic_write(path: &Path, body: &str) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if !parent.as_os_str().is_empty() {
        let _ = fs::create_dir_all(parent);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if fs::write(&tmp, body).is_err() {
        return false;
    }
    let _ = fs::remove_file(path);
    fs::rename(&tmp, path).is_ok()
}

/// 写清单（槽位全部写成功后调用——清单存在即「本次会话可恢复」的原子信号）。
/// `pub(crate)`：供测试直接驱动显式临时目录。
pub(crate) fn write_manifest(dir: &Path, m: &SessionManifest) -> bool {
    atomic_write(&manifest_path(dir), &m.to_value().to_string())
}

/// 读清单：文件缺失 / JSON 损坏 / 非对象 → None（不可恢复）。
pub(crate) fn read_manifest(dir: &Path) -> Option<SessionManifest> {
    let text = fs::read_to_string(manifest_path(dir)).ok()?;
    let v = serde_json::from_str::<Value>(&text).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(SessionManifest::from_value(&v))
}

/// 写槽位内容（LF 规范化；空内容也写空文件）。`pub(crate)`：供测试直接驱动。
pub(crate) fn write_slot(dir: &Path, slot: &str, content: &str) -> bool {
    // 快速路径：无 \r → 内容直接原子写（编辑内容内部已 LF，绝大多数情况命中，省两次整串拷贝）。
    if !content.as_bytes().contains(&b'\r') {
        return atomic_write(&slot_path(dir, slot), content);
    }
    let lf = content.replace("\r\n", "\n").replace('\r', "\n");
    atomic_write(&slot_path(dir, slot), &lf)
}

/// 读槽位内容（缺失 → None）。`pub(crate)`：供测试直接驱动。
pub(crate) fn read_slot(dir: &Path, slot: &str) -> Option<String> {
    fs::read_to_string(slot_path(dir, slot)).ok()
}

/// 清会话：删清单 + 尽量清掉清单引用的槽位文件（幂等；单项失败不影响其余）。
/// `pub(crate)`：开关关闭与恢复完成共用，测试直接驱动。
pub(crate) fn clear_session(dir: &Path) -> bool {
    if let Some(m) = read_manifest(dir) {
        for tab in &m.tabs {
            if let SessionTab::New { slot, .. } = tab {
                let _ = fs::remove_file(slot_path(dir, slot));
            }
        }
    }
    let _ = fs::remove_file(manifest_path(dir));
    true
}

/// 清理孤儿槽位（纯函数，可测）：删除 `dir` 内**不是** `session.json`、**不是**
/// `manifest.tabs[].New.slot` 引用、且**非 `.tmp` 后缀**的文件（即陈旧 tN 槽位）。
/// `remove_tab` 后即时持久化会重编号未命名槽位，被关选项卡的旧槽位若不复用即成
/// 孤儿——本函数保证「否→删除临时文件」真正落盘 + 根治孤儿泄漏。`dir` 不存在 /
/// 读不出 → 安全返回 true（不 panic、无操作）。
pub(crate) fn prune_unreferenced_slots(dir: &Path, manifest: &SessionManifest) -> bool {
    let mut referenced: HashSet<&str> = HashSet::new();
    for tab in &manifest.tabs {
        if let SessionTab::New { slot, .. } = tab {
            referenced.insert(slot.as_str());
        }
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == "session.json" || name.ends_with(".tmp") || referenced.contains(name.as_str()) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }
    true
}

impl EditorApp {
    /// 「恢复上次会话」开关：翻转 + 立即持久化（不等退出）。关闭时顺带清掉遗留
    /// 会话清单/槽位，避免日后重新开启时恢复陈旧会话。
    pub(super) fn toggle_session_restore(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.session_restore = !self.session_restore;
        // 与其它偏好同款落盘（persist_prefs 已纳入 session_restore 字段）。
        self.persist_prefs();
        if !self.session_restore {
            // 清槽位指纹：重开时若沿用旧指纹会误跳过写槽位（盘面已被清，内容却判「未变」），
            // 导致恢复读到缺失槽位 → 空内容。测试构建不触碰真实会话目录（clear 走显式目录的
            // 纯逻辑），但指纹表在两种构建都清（save_session_to 关闭分支同样兜底）。
            self.session_slot_hashes.clear();
            self.session_last_manifest_digest = None;
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
    /// `#[cfg(not(test))]`：见 [`Self::save_session`]。
    #[cfg(not(test))]
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = restore_session_from(self, &UserPrefs::session_dir(), window, cx);
    }

    /// 把当前主窗口选项卡集合**立即**持久化到会话目录（连续持久化核心：关闭程序时
    /// 状态本就在盘上，天然覆盖全部退出路径——退出钩子在部分平台不触发）。
    /// 仅在非 detached、开关开启、非恢复期间落盘；测试构建恒跳过（**绝不**触碰真实
    /// 会话目录，验证走显式临时目录的纯逻辑 [`save_session_to`]，见 `tests/session.rs`）。
    pub(super) fn persist_session(&mut self, cx: &mut Context<Self>) {
        if self.is_detached
            || !self.session_restore
            || self.session_restoring
            // cfg!(test) 为编译期常量，测试构建恒短路早退；磁盘写不触碰真实 APPDATA。
            || cfg!(test)
        {
            return;
        }
        let _ = save_session_to(self, &UserPrefs::session_dir(), cx);
    }

    /// 安排一次会话持久化：结构变化 `immediate=true` 立即写盘；内容变化 `false` 走
    /// [`SESSION_FLUSH_DEBOUNCE_MS`] 防抖（打字停顿后才写一次，不逐键写盘）。仿侧边栏
    /// 搜索防抖（`schedule_search_debounce`）的时序代号模式：旧防抖定时器到点比对
    /// 不符即作废，防抖可重入。防抖期间又被安排 → 新代号覆盖，最终只落盘一次最新状态。
    /// 超时 / 唤醒失败静默丢弃，不 panic。
    pub(super) fn schedule_session_flush(&mut self, cx: &mut Context<Self>, immediate: bool) {
        if immediate {
            self.persist_session(cx);
            return;
        }
        let seq = self.session_flush_seq.wrapping_add(1);
        self.session_flush_seq = seq;
        // 防抖只挂在非测试构建：测试直接驱动纯逻辑函数验证，不在测试进程残留定时任务。
        #[cfg(not(test))]
        {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(SESSION_FLUSH_DEBOUNCE_MS))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    // 防抖窗内又安排过 → 序号已变，本定时器作废（只落盘最新一次）。
                    if this.session_flush_seq == seq {
                        this.persist_session(cx);
                    }
                });
            })
            .detach();
        }
    }
}

/// 内容指纹：u64 哈希（`std` 默认 Hasher / SipHash-1-3，无新增依赖）。仅用于「内容未变
/// 则跳过槽位重写」的判定；撞车率 ~2^-64，且被跳过时盘面恰是本次内容（此前自身写入的
/// 产物），碰撞失真的风险可忽略。
fn content_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// 保存会话（纯逻辑，显式目录，可测试）：开关关闭 → 只清不做；否则先写槽位内容、
    /// 再原子写清单（清单是"本次会话可恢复"的原子信号）。文件选项卡只记路径，未命名
    /// 选项卡经 [`EditorApp::tab_effective_text`]（与手动保存同口径：live2/WYSIWYG 开启
    /// 时读 WyEditor 实时内容）取内容写槽位。
    /// **跳过未变槽位**：以「槽位名 + 内容 hash」对照 [`EditorApp::session_slot_hashes`]，
    /// 内容未变的槽位不重写整份文件——切换选项卡 / 结构变化（旧实现每次都全量重写全部
    /// 未命名内容并在主线程同步磁盘 IO）摊薄到只剩一份清单写 + 目录扫描；仅真正变化的
    /// 槽位才写。槽位名随未命名顺序位移时（关选项卡重编号）名不同 → 照写，旧槽位交给
    /// [`prune_unreferenced_slots`] 清理。
    /// 写清单成功后顺带 [`prune_unreferenced_slots`] 清理不再被引用的陈旧槽位
    /// （`remove_tab` 后重编号产生的孤儿），保证「否→删除临时文件」真正落盘。
    pub(crate) fn save_session_to(app: &mut EditorApp, dir: &Path, cx: &App) -> bool {
        if !app.session_restore {
            // 开关关闭：清盘面 + 清指纹（重开时旧指纹会误判「盘上有同内容」而跳过写槽位
            // → 恢复读到缺失槽位为空内容；此处一并清掉，杜绝该竞态）。
            app.session_slot_hashes.clear();
            app.session_last_manifest_digest = None;
            return clear_session(dir);
        }
        let mut tabs = Vec::new();
        let mut n_slots = 0usize;
        for (idx, tab) in app.tabs.iter().enumerate() {
            match &tab.file_path {
                Some(p) => tabs.push(SessionTab::File { path: p.clone() }),
                None => {
                    let slot = format!("t{n_slots}");
                    n_slots += 1;
                    let text = app.tab_effective_text(idx, cx);
                    let h = content_hash(&text);
                    // 槽位名与指纹都未变 → 内容与盘面一致，跳过重写（消除切选项卡/结构
                    // 变化时的全量同步磁盘写）。写失败仅当确实要写时返回（跳过不影响盘面）。
                    let unchanged = app.session_slot_hashes.get(&slot).copied() == Some(h);
                    if !unchanged {
                        if !write_slot(dir, &slot, &text) {
                            return false;
                        }
                        app.session_slot_hashes.insert(slot.clone(), h);
                    }
                    tabs.push(SessionTab::New {
                        slot,
                        name: "未命名".to_string(),
                    });
                }
            }
        }
        // 收敛指纹表规模：本次不再存在的旧槽位（选项卡被关/重编号后）直接淘汰，
        // 避免 HashMap 随「反复新建/关闭未命名选项卡」长期累积（内存不放大）。
        app.session_slot_hashes.retain(|slot, _| {
            slot.strip_prefix('t')
                .and_then(|s| s.parse::<usize>().ok())
                .map_or(false, |n| n < n_slots)
        });
        let manifest = SessionManifest {
            version: 1,
            active_tab: app.active_tab,
            tabs,
        };
        // 清单是"本次会话可恢复"的原子信号，但写它恰是切选项卡/新建时最贵的原语
        // （本机防病毒下原子写 ~5-25ms）。清单未变（`new_tab` 紧随 `make_tab` 的二次
        // 即时落盘、内容防抖收敛后的等价重写等「无变化重入」）→ 字节级相同，盘面已最
        // 新，直接跳过清单写 + prune（零文件 IO）；槽位已在上面写完。
        let body = manifest.to_value().to_string();
        let digest = content_hash(&body);
        if app.session_last_manifest_digest == Some(digest) {
            return true;
        }
        if !write_manifest(dir, &manifest) {
            return false;
        }
        app.session_last_manifest_digest = Some(digest);
        let _ = prune_unreferenced_slots(dir, &manifest);
        true
    }

/// 恢复会话（纯逻辑，显式目录，可测试）：读清单 → 依次开文件 / 建未命名选项卡并
/// 灌槽位内容 → 活动选项卡按清单取并 clamp 到 `[0, tabs.len())` → 删除清单并清理
/// 槽位（避免重复恢复）。开关关闭 / 清单缺失 → false（不动作）。文件已不存在 → 跳过
/// （与 CLI 首参打开同口径：`is_file` 才开）。
pub(crate) fn restore_session_from(
    app: &mut EditorApp,
    dir: &Path,
    window: &mut Window,
    cx: &mut Context<EditorApp>,
) -> bool {
    if !app.session_restore {
        return false;
    }
    let Some(manifest) = read_manifest(dir) else {
        return false;
    };
    // 恢复期间抑制持久化：重建选项卡 / 灌内容不触发写盘（避免反复落盘 / 抵抗尾部
    // `clear_session` 清理）。
    app.session_restoring = true;
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
    app.active_tab = if app.tabs.is_empty() {
        0
    } else {
        manifest.active_tab.min(app.tabs.len() - 1)
    };
    // 恢复完成放行持久化，随后既有 `clear_session` 删除清单/槽位（避免重复恢复）。
    app.session_restoring = false;
    clear_session(dir);
    // 盘面已被清（槽位/清单删除）：指纹表也必须清，否则此后首次持久化会拿「恢复前内容」
    // 的旧指纹误判「未变」而跳过写槽位/清单 → 恢复出的选项卡内容不再落盘。
    app.session_slot_hashes.clear();
    app.session_last_manifest_digest = None;
    app.status = SharedString::from("已恢复上次会话");
    app.status_ok = true;
    cx.notify();
    true
}
