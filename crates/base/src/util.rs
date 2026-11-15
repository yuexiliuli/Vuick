//! 全 crate 共享的小助手。
//!
//! - **UTF-8 字符边界钳制基元**（纯函数，无副作用）：Markdown 预览
//!   `markdown::view::row`、WYSIWYG `markdown::wysiwyg::shape::runs_to_text_runs`、
//!   源码编辑器 `editor::view::render::char_align_highlights` 三处此前各自内联的
//!   「向前进位吸收到字符边界」逻辑统一收敛到这里（M1.1-P5 三处合一）。
//! - **[`SeqAfter`] 延迟防抖、代序作废助手**（依赖 gpui 调度）：收敛 shell 内四处
//!   「spawn timer + 单调 seq + 比对作废」手抄骨架。

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{App, Context, WeakEntity};

/// 把字节位置**向前进位**钳到最近的 **UTF-8 字符边界**：先钳入 `text` 长度，再从 `p`
/// 起向后扫描到 `is_char_boundary` 为真的位置（`p` 已在边界上则原样返回）。
///
/// 这是「run/区间不能切在多字节字符里」的**共享基元**：CJK/emoji/组合字符下，
/// 编辑→重析后按字节搬运的区间会把边界切进字符内部，GPUI `Text`/`StyledText`
/// 在非字符边界处切 run 会 panic（`text.rs` 的 `is_char_boundary` assert / shaping 崩）。
/// 预览 `row::char_align_range`、WYSIWYG `shape::runs_to_text_runs`、源码编辑器
/// `render::char_align_highlights` 三处统一收敛到它。
pub fn char_align_byte(text: &str, mut p: usize) -> usize {
    let len = text.len();
    p = p.min(len);
    while p < len && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// 延迟防抖、代序作废助手。
///
/// 收敛 shell 内四处「spawn timer + 单调代序 + 比对作废」手抄骨架（sidebar 搜索防抖 /
/// session 防抖 / tab_bar 横向滚动条隐藏 / tooltip 延迟）：调用 [`SeqAfter::after`] 排一单
/// 「延时后执行」；**同一实例再次触发会把在途旧单作废**——到点执行前先比当代序，期间
/// 又有新触发（代序已前进）则旧单直接丢弃、不执行旧动作。
///
/// # 用法
/// 调用方在自己的状态里持有一个 `SeqAfter`（如侧栏搜索防抖槽位），在事件处理器里：
/// ```ignore
/// state.update(cx, |s, cx| s.search_debounce.after(cx, Duration::from_millis(150), { ... }));
/// ```
/// `action` 经 `AsyncApp::update` 送回**主线程**执行；它须 `Send + 'static`（常见捕获
/// `Weak<Entity<_>>` / `Arc`，勿捕获 `Rc`）；宿主实体已释放时建议在 action 里经 weak
/// 自行空操作（与 `ui::tooltip` 同一姿势）。
#[derive(Default)]
pub struct SeqAfter {
    /// 单调代序号。主线程累加、后台线程在到点后回读比对：原子防撕裂（每次触发 +1，
    /// 旧单永久失配）。
    seq: Arc<AtomicU64>,
}

impl SeqAfter {
    /// 新实例（首次触发代序 = 1；0 表示从未触发）。
    pub fn new() -> Self {
        Self {
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 当前代序号（测试断言 / 诊断用）。
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// 触发一次延迟动作：`delay` 后若期间无更新触发则执行 `action`；期间再触发
    /// （含多轮重入）只让**最新一单**生效——旧单到点时代序已前进、比对失配即作废。
    pub fn after<F>(&self, cx: &mut App, delay: Duration, action: F)
    where
        F: FnOnce(&mut App) + Send + 'static,
    {
        // 单调代序：本单捕获「当前值 + 1」（每次触发都推进，先前的单永远比不中）。
        let seq = self.seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let counter = self.seq.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            // 送回主线程执行；代序未前进（本单仍是最新）才真正跑 action。
            cx.update(|app| {
                if counter.load(Ordering::SeqCst) == seq {
                    action(app);
                }
            });
        })
        .detach();
    }

    /// 触发一次延迟动作，经**实体弱句柄**升级后执行：`delay` 后若期间无更新触发，把 `weak`
    /// 升级为强句柄并在实体仍存活时执行 `action`（实体已释放 → 升级失败 → 静默丢弃，不 panic）。
    ///
    /// 这是「`cfg(not(test))` 门 + `weak_entity` 升级 + 静默丢弃」手抄骨架（session 落盘 /
    /// reading_progress / media_size / sidebar 过滤 / tooltip 防抖）的收敛形式：调用方持
    /// `SeqAfter` 槽位、给一个 `cx.weak_entity()` 与 `action`（= 原回调里 `weak.update(app, …)`
    /// 的内层闭包原样搬入），即可把防抖接线收敛成一行。
    ///
    /// `H` 为宿主实体类型（`weak` 的来源实体）；`action` 签名 `(&mut H, &mut Context<H>)` 与原
    /// `weak.update(app, |this, cx| …)` 内层闭包逐参数等价。动作经 `AsyncApp::update` 送回
    /// **主线程**执行（`Send + 'static`），代序作废语义同 [`Self::after`]。
    /// `cfg(not(test))` 门**由调用方各自负责**：`test` 语义属 per-crate（各 crate 按「测试
    /// 进程是否残留定时任务」自己决定是否把调度块关进非测试构建），本方法不做内部门控。
    pub fn after_weak<H: 'static>(
        &self,
        cx: &mut App,
        delay: Duration,
        weak: WeakEntity<H>,
        action: impl FnOnce(&mut H, &mut Context<H>) + Send + 'static,
    ) {
        self.after(cx, delay, move |app| {
            let _ = weak.update(app, action);
        });
    }
}

/// `Element::source_location` 的统一载体：gpui 自定义元素一律**不**上报源码位置
/// （inspector 里无从定位）。
///
/// ui::input::InputElement / ui::icon::Icon / ui::scrollbar::Scrollbar /
/// editor::view::render::EditorElement / markdown-wysiwyg::WyEditorElement 五处此前各自
/// 手抄 `None`，收敛为这里的公共函数，各 impl 委托它（gpui `Element` trait 该方法
/// 无默认体，impl 方法不可省）。
pub fn source_location() -> Option<&'static core::panic::Location<'static>> {
    None
}

/// 「同目录临时文件 + rename」原子写：同目录建唯一名临时文件（进程 pid + 单调纳秒
/// 时间戳）写入并 `sync_all` 落盘后 `rename` 覆盖目标。
///
/// `std::fs::rename` 在 Windows 走 MoveFileEx 可覆盖既有目标——**不要**先删旧文件再
/// rename（先删后改存在「旧文件已删、新文件未就位」的非原子窗口，中途崩溃丢旧文件）。
/// 唯一临时名（pid + 纳秒时间戳）让并发写入者互不覆盖彼此的临时文件；同一目标并发写
/// 时最后一次 rename 胜出，目标始终是某个完整文件。
/// config::user::save 与 shell::app::session::atomic_write 两处此前各自手抄此套路，收敛至此。
/// 错误语义：写失败会清理半截临时文件并把 `io::Error` 回传（`fs::rename` 失败同样回传）。
pub fn atomic_write<P: AsRef<Path>>(path: P, bytes: &[u8]) -> io::Result<()> {
    let path = path.as_ref();
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)?;
    }
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic_write: 目标路径必须以文件名结尾",
        )
    })?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = match parent {
        Some(parent) => parent.join(format!(
            "{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            nanos
        )),
        None => PathBuf::from(format!(
            "{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            nanos
        )),
    };
    let write = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?; // flush 落盘（比 flush 更强：fsync 到磁盘）
        Ok(())
    })();
    if let Err(err) = write {
        // 写失败清理半截临时文件，避免残留；错误交还调用方处置。
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 时长（秒，f64）→ 时钟字符串。负值 / 非有限 / 0 一律按 0 处理（`"0:00"`）。
///
/// 格式与 video `fmt_time` 对齐（收敛自播放器双份独立实现）：
/// - `< 1h`：`m:ss`（分不补零、秒两位补零）；
/// - `≥ 1h`：`h:mm:ss`（小时不补零、分/秒两位补零），如 `1:01:23`。
///
/// 收敛前 music `duration_str` 无小时分支（≥1h 显示 `61:23` 这类 m:ss，属隐含 bug），
/// 统一后按此处语义走 h:mm:ss——这是**显式语义统一（修隐含 bug）**，非回归。
pub fn format_duration(secs: f64) -> String {
    let secs = if secs.is_finite() && secs > 0.0 { secs } else { 0.0 };
    let total = secs as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{App, TestAppContext};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    /// M1.1-P5 上游坏区间诊断：任何切进多字节字符（CJK / emoji / ZWJ 序列 / 组合字符 /
    /// 越界）内部的字节偏移，喂给统一后的共享基元都必须**不 panic** 且被正确钳制到
    /// 最近的字符边界。这是预览 `row` / WYSIWYG `shape` / 源码编辑器 `render` 三处共用
    /// 的同一路径，链路端到端的字形测试分别在 `row::tests::char_align_range_absorbs_cjk_boundaries`、
    /// `element::tests::runs_to_text_runs_clamps_mid_char_ranges_to_char_boundaries`、
    /// `editor::tests::char_align_highlights_survives_stale_ranges`。
    #[test]
    fn char_align_byte_absorbs_mid_char_offsets() {
        // "测🍎试❗"：测=3B，🍎=4B，试=3B，❗=3B → 13 字节。
        let text = "测🍎试❗";
        assert_eq!(text.len(), 13);
        for (p, expected) in [
            (1, 3),   // 切进『测』(0..3) 内部 → 前向吸收到 3
            (2, 3),   // 同上
            (4, 7),   // 切进 🍎(3..7) 内部 → 吸收到 7
            (6, 7),   // 同上
            (8, 10),  // 切进『试』(7..10) 内部 → 吸收到 10
            (11, 13), // 切进 ❗(10..13) 内部 → 吸收到 13
        ] {
            assert_eq!(char_align_byte(text, p), expected, "p={p}");
        }
        // 已对齐 / 恰好 len / 越界 → 原样或钳到 len，一律不 panic。
        assert_eq!(char_align_byte(text, 3), 3);
        assert_eq!(char_align_byte(text, 13), 13);
        assert_eq!(char_align_byte(text, 999), 13);
    }

    #[test]
    fn char_align_byte_survives_emoji_zwj_and_combining() {
        // ZWJ 家庭分组 emoji：任何非边界内部偏移都被吸收到真字符边界。
        let zwj = "👨\u{200d}👩\u{200d}👧";
        for off in 1..zwj.len() {
            let aligned = char_align_byte(zwj, off);
            assert!(zwj.is_char_boundary(aligned), "ZWJ off={off} → {aligned}");
            assert!(aligned >= off, "只能向前进位");
        }
        // 组合字符：cafè = c/a/f/e + U+0301 组合重音（字节 4..6），end=5 切进组合内 → 吸收到行尾。
        let comb = "cafe\u{301}";
        let aligned = char_align_byte(comb, 3)..char_align_byte(comb, 4 + 1);
        assert_eq!(aligned, 3..comb.len(), "end 应吸收到组合字符序列末尾: {aligned:?}");
        assert!(comb.is_char_boundary(aligned.start) && comb.is_char_boundary(aligned.end));
    }

    /// 区间语义（两端的字节偏移各自吸收到字符边界）：空/越界/反向安全，
    /// 可能坍缩为 0 长（调用方自行丢弃空段）。
    #[test]
    fn char_align_range_absorbs_mixed_and_out_of_bounds() {
        let text = "你说得对，好。行不行。";
        let align = |s: usize, e: usize| char_align_byte(text, s)..char_align_byte(text, e);
        // 终点 19 切在「。」(18..21) 中间 → 前向吸收到 21；起点 18 已在边界上。
        assert_eq!(align(0, 19), 0..21);
        // 已对齐的不变。
        assert_eq!(align(3, 6), 3..6);
        // end 越界 → 钳入行尾。
        assert_eq!(align(18, 999), 18..text.len());
        // 整体越界 → 不 panic、坍缩为同边界（调用方丢弃空段）。
        assert_eq!(align(500, 999), text.len()..text.len());
    }

    /// 时长格式化（video/music 播放器统一入口）：边界与进位逐一断言。
    #[test]
    fn format_duration_covers_edges_and_hours() {
        // 边界清单：0 / 不足 1 分 / 整分 / 1h 前最后 1 秒 / 整点 1h / 跨 1h 进位。
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(59.9), "0:59");
        assert_eq!(format_duration(60.0), "1:00");
        assert_eq!(format_duration(3599.0), "59:59");
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3661.0), "1:01:01");
        // 负值 / 非有限一律按 0 处理（video `fmt_time` 与 music `duration_str` 原有语义一致）。
        assert_eq!(format_duration(-5.0), "0:00");
        assert_eq!(format_duration(f64::NAN), "0:00");
        assert_eq!(format_duration(f64::INFINITY), "0:00");
    }

    /// 幂等 / 基本时序：单次触发 → 未到点不执行、到点恰执行一次、此后不再重复。
    #[gpui::test]
    async fn seq_after_fires_exactly_once_after_delay(cx: &mut TestAppContext) {
        let counter: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let debounce = SeqAfter::new();
        let action = {
            let counter = counter.clone();
            move |_app: &mut App| {
                counter.fetch_add(1, AtomicOrdering::SeqCst);
            }
        };
        cx.update(|app| debounce.after(app, Duration::from_millis(50), action));
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 0, "未到点不得执行");
        cx.executor().advance_clock(Duration::from_millis(30));
        cx.run_until_parked();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 0, "30ms < 50ms 仍不执行");
        cx.executor().advance_clock(Duration::from_millis(30));
        cx.run_until_parked();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1, "到点恰执行一次");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1, "过时不重复执行（幂等）");
        assert_eq!(debounce.seq(), 1, "单次触发代序 = 1");
    }

    /// 代序作废 / 可重入：A 触发后、到点前再触发 B —— A 到点时已被作废（不执行），
    /// 只有最新代序的 B 在它自己的延迟窗口后执行。
    #[gpui::test]
    async fn retrigger_invalidates_previous(cx: &mut TestAppContext) {
        let a: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let b: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let debounce = SeqAfter::new();
        // A：t=0 触发，窗口 50ms → 应于 t=50 到点。
        let a_action = {
            let a = a.clone();
            move |_app: &mut App| {
                a.fetch_add(1, AtomicOrdering::SeqCst);
            }
        };
        cx.update(|app| debounce.after(app, Duration::from_millis(50), a_action));
        // t=20：A 未到点，重入触发 B（窗口 50ms → 应于 t=70 到点）。
        cx.executor().advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let b_action = {
            let b = b.clone();
            move |_app: &mut App| {
                b.fetch_add(1, AtomicOrdering::SeqCst);
            }
        };
        cx.update(|app| debounce.after(app, Duration::from_millis(50), b_action));
        // t=60：已越过 A 的到点（50），B（70）未到 —— A 被作废、B 待执行。
        cx.executor().advance_clock(Duration::from_millis(40));
        cx.run_until_parked();
        assert_eq!(a.load(AtomicOrdering::SeqCst), 0, "A 到点时代序已被 B 前进 → 作废不执行");
        assert_eq!(b.load(AtomicOrdering::SeqCst), 0, "B 尚未到点");
        // t=110：B 到点 → 执行一次；A 永不执行。
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        assert_eq!(a.load(AtomicOrdering::SeqCst), 0, "旧代序动作永不执行");
        assert_eq!(b.load(AtomicOrdering::SeqCst), 1, "最新代的 B 到点执行一次");
        assert_eq!(debounce.seq(), 2, "两次触发代序推进到 2");
    }
}
