//! 编辑区 ⇄ 预览滚动同步。
//!
//! 同步单位 = 源码行所在**块起点**（两侧都经 `WyEditor::block_start_of_line` /
//! `WyEditor::top_source_line` 归一成块）：同一块（段落/代码块/标题）内微滚动双方键不变、
//! 不互拉，避免反馈抖动；跨块才驱动对端。逻辑：编辑器顶行块变了 → 预览
//! `scroll_to_line` 到该块；预览顶块变了（且编辑器未同时动）→ 编辑器滚动到该块起点。
//! 双侧收敛到同一块后静止。含 I2「Scrub 延后」收敛状态机（快速跨块滚屏期编辑器跟随
//! 延后到静止后一次收敛）。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::App;
use shell_core::{MdScrollSync, AppModel};

/// I2：右栏预览「Scrub 延后」进入判定的连续跨块帧下限。预览连续 ≥ 此帧数快速跨块
/// （右栏拖滚动条 / 高频滚屏）即判定为 Scrub 期——期内的编辑区 `scroll_to_line` 跟随
/// **延后**到静止后一次收敛，消除「拖动每帧嵌编辑器全窗重建」的 28ms/帧卡顿
/// （编辑区 build_rows+prepaint+paint ≈11ms 叠加 WYSIWYG ~17ms）。低频单步滚屏
/// 跨块数不足、不触发，保持慢滚逐帧对齐。
pub const SCRUB_ENTER_STREAK: u32 = 2;
/// Scrub 延后期内「预览静止」的收敛帧数：预览连续 ≥ 此帧数不再跨块（拖条松手 /
/// 快速连滚停下）即**一次收敛**到延后期内最新预览顶块。帧数与墙钟兜底二者取先到。
pub const SCRUB_IDLE_FRAMES: u32 = 2;
/// Scrub 延后期内「预览静止」的收敛墙钟兜底：即使渲染帧稀疏（静止后无持续重绘），
/// 距上次跨块 ≥ 此时长后任一后续同步帧都会一次收敛，避免两栏长期错位。
pub const SCRUB_IDLE: std::time::Duration = std::time::Duration::from_millis(200);

/// 编辑器被预览驱动后的落位收敛宽限（帧）：边界块（表格等）顶行读取可能滞后/抖动
/// 一两帧，此期内「编辑器落位」不回显为「编辑器主动滚动」；超过才放手由正常双向同步接管。
pub const DRIVEN_EDITOR_MAX_AWAIT: u32 = 8;

/// 编辑区⇄预览滚动同步（每帧渲染开头调用）。只在「活动选项卡以 WyEditor 作 MD
/// 预览且在**非 live2 实时编辑**的分屏（非全屏 / 非只读 + 预览开启）」时生效——
/// live2 下右栏预览不在元素树，驱动隐藏实体只会残留陈旧目标。
///
/// 同步单位 = 源码行所在**块起点**（两侧都经 [`WyEditor::block_start_of_line`] /
/// [`WyEditor::top_source_line`] 归一成块）：同一块（段落/代码块/标题）内微滚动
/// 双方键不变、不互拉，避免反馈抖动；跨块才驱动对端。
/// 逻辑：编辑器顶行块变了 → 预览 `scroll_to_line` 到该块；预览顶块变了（且编辑器
/// 未同时动）→ 编辑器滚动到该块起点。双侧收敛到同一块后静止。
///
/// 注意：两侧键读取的是**上一帧**的滚动/视口状态（`top_source_line` 读 `panes`
/// 上一帧渲染期推入的 `viewport_top_px`）——本帧推入发生在 sync 之后，故同步天然
/// 滞后一帧；对逐帧收敛无影响，反而更稳。
///
/// `state` = 宿主持有的 `MdScrollSync` 基线（builder 值类型在 shell_core；
/// `cx` 只用于读/写编辑区与预览实体，无需宿主服务。
pub fn sync_editor_preview_scroll(
    state: &mut MdScrollSync,
    model: &Rc<RefCell<AppModel>>,
    cx: &mut App,
) {
    let idx = model.borrow().active_tab;
    if crate::mdwy::md_wy_preview(&model.borrow()).is_none() {
        return;
    }
    let m = model.borrow();
    let tab = &m.tabs[idx];
    if tab.payload.readonly_preview || tab.payload.preview_fullscreen || !crate::mdwy::show_preview(&m) || tab.live_edit {
        return;
    }
    let Some(md) = tab.payload.md_wy.clone() else { return };
    let editor = tab.payload.editor.clone();

    // 读两侧当前块起点键（预览帧未就绪 → None，本帧不动）。
    let preview_block = md.read(cx).top_source_line();
    let editor_block = Some(md.read(cx).block_start_of_line(editor.read(cx).viewport_top_line()));

    // 首次进入该选项卡 / 换文档（块号失义）/ 编辑器内容已编辑（块映射变）：
    // 重建基线，只记录不驱动。
    let ev = Some(editor.read(cx).version());
    if state.tab != idx || state.editor_version != ev {
        *state = MdScrollSync {
            tab: idx,
            editor_version: ev,
            editor_block,
            preview_block,
            driven_preview_target: None,
            driven_editor_target: None,
            driven_editor_await: 0,
            scrub: false,
            scrub_streak: 0,
            scrub_idle_frames: 0,
            scrub_target: None,
            scrub_last_cross: std::time::Instant::now(),
        };
        return;
    }
    let prev = *state;
    state.editor_block = editor_block;
    state.preview_block = preview_block;

    // I2 Scrub 检测前置量：预览本帧是否「自主跨块」（顶块较上帧变化，且**不是**编辑器
    // 主导下的预览到达——编辑器驱动预览期 `driven_preview_target=Some`，到达帧绝不计入
    // Scrub，防编辑器→预览对齐被误判成拖动）。逐帧维护连续跨块计数（非连续即清零），
    // 供进入延后判定用；同时刷新墙钟兜底时间戳。
    let preview_moved = state.driven_preview_target.is_none()
        && prev.preview_block != preview_block
        && preview_block != state.driven_preview_target;
    if preview_moved {
        state.scrub_streak = state.scrub_streak.saturating_add(1);
        state.scrub_last_cross = std::time::Instant::now();
    } else {
        state.scrub_streak = 0;
    }

    let Some(eb) = editor_block else { return };
    if preview_block == Some(eb) {
        // 两侧同一块，已同步：作废「等待预览到达」与「等待编辑器到达」的驱动目标（已对齐）。
        // Scrub 延后态一并退出（preview_moved=false 帧已把 streak 清零）。
        state.driven_preview_target = None;
        state.driven_editor_target = None;
        state.driven_editor_await = 0;
        state.scrub = false;
        state.scrub_idle_frames = 0;
        state.scrub_target = None;
        return;
    }
    // 我方正在驱动编辑器到达 `driven_editor_target`：期间 editor_block 的连续变化是
    // 跟随落位（边界块顶行读取还会滞后/抖动一两帧），**不是**用户主动滚动 → 不得触发
    // 下方「编辑器先动 → 驱动预览」回声，否则预览会被编辑器落位拽回（来回跳动）。
    // 宽限期内只计数，超限才放手交给正常双向同步（防永远到不了目标时停摆）。
    if prev.editor_block != editor_block {
        if state.driven_editor_target.is_some() {
            // 我方驱动编辑器落位中（含 Scrub 收敛 / 进入延后前的最后一次跟随落位）：
            // 抑制回声，只给宽限计数；超限才放手交给正常双向同步。**不**清 Scrub——
            // 延后期内编辑器落位余波不得把延后态打断（防拖动中延后被落位逐帧打断）。
            // 注意：放手时**不得**顺便重钉预览（会把预览拉回，重蹈回声之弊）——清标后由
            // 后续帧的正常判定接管（真用户滚编辑器 → 下一帧 editor_block 再变 → 正常驱动
            // 预览；边界块卡住 → 编辑器不再变 → 安静保持，不再互拉）。
            if state.driven_editor_await >= DRIVEN_EDITOR_MAX_AWAIT {
                state.driven_editor_target = None;
                state.driven_editor_await = 0;
            } else {
                state.driven_editor_await += 1;
            }
            return;
        }
        // 编辑器真正用户滚动（无我方驱动在途且不在 Scrub 延后期）→ 预览跟到编辑器
        // 顶行所在块；Scrub 延后不适用，退出。
        state.scrub = false;
        state.scrub_idle_frames = 0;
        state.scrub_target = None;
        // **不能在本帧 render 直接调** `scroll_to_line`（宿主 `ScrollHandle::set_offset`
        // 在渲染期会被本帧布局复位）：只把目标行写进 WY 的 `scroll_target`，由其元素
        // prepaint 逐帧重钉宿主偏移直到稳住（有 [`WyEditor::SCROLL_TARGET_MAX_APPLIES`]
        // 兜底）。记录「等待预览到达」目标：预览重钉落位到它时不被误判为预览用户滚动。
        md.update(cx, |v, _| v.set_scroll_target(eb));
        state.driven_preview_target = Some(eb);
    } else if state.scrub && !preview_moved {
        // Scrub 延后期内预览已静止（拖条松手 / 连滚停下）：静止帧计数推进，达到帧数
        // 或墙钟阈值（二者取先到）即**一次收敛**到延后期内最新预览顶块，恢复双侧对齐
        // （收敛后由正常双向同步照常跟进——幂等，目标已到即静止）。
        state.scrub_idle_frames += 1;
        if state.scrub_idle_frames >= SCRUB_IDLE_FRAMES
            || state.scrub_last_cross.elapsed() >= SCRUB_IDLE
        {
            if let Some(t) = state.scrub_target {
                if tab.payload.md_restore_pending.is_none() {
                    editor.update(cx, |ed, _| ed.scroll_to_line(t));
                    state.driven_editor_target = Some(t);
                    state.driven_editor_await = 0;
                    state.editor_block = Some(t);
                    state.driven_preview_target = None;
                } else {
                    shell_core::rp_diag::diag("sync_guard_blocked", "site=scrub_converge");
                }
            }
            state.scrub = false;
            state.scrub_idle_frames = 0;
            state.scrub_target = None;
        }
    } else if preview_moved {
        // 预览自主跨块 → 预览主导。统一收尾：旧等待预览目标作废。
        state.driven_preview_target = None;
        if state.scrub {
            // 延后期内继续跨块：只推进收敛目标（最新预览顶块），**不逐帧**驱动编辑器
            // （延后的全部意义所在：拖动中不再每帧嵌编辑器全窗重建）。`driven_editor_target`
            // 保持不动——延后期前的最后一次跟随落位（若有）仍由 `driven_editor_await` 收敛。
            // **不预填 editor_block**：延后期内编辑器未被驱动，顶块就按实读数，避免伪造
            // 「编辑器在动」触发上方分支二、把延后态打断。
            state.scrub_target = preview_block;
            state.scrub_idle_frames = 0;
            return;
        }
        if state.scrub_streak >= SCRUB_ENTER_STREAK {
            // 连续跨块（拖动 / 高频滚屏）→ 进入 Scrub 延后：本次起编辑器跟随延后到
            // 静止后一次收敛。首/低频单次跨块不满足连跨阈值，仍走下方立即跟随。
            // 同理不预填 editor_block（编辑器未被驱动，看实读数）。
            state.scrub = true;
            state.scrub_idle_frames = 0;
            state.scrub_target = preview_block;
            return;
        }
        // 首次/低频跨块：照常立即跟随（同块内微滚 / 慢速单步滚仍逐帧对齐，行为不变）。
        // md 阅读进度恢复窗口（`tab.md_restore_pending` 在途）例外：编辑区恢复位是这段的
        // 权威面，预览首次就绪/装配抖动读到顶部块正是「把恢复行拉回顶部」的触发源
        // （rp_diag 49→2）——守卫期不跟随，由 `advance_md_restore_window` 收敛后归还。
        if let Some(pb) = preview_block {
            if tab.payload.md_restore_pending.is_none() {
                editor.update(cx, |ed, _| ed.scroll_to_line(pb));
                // 预置编辑器基线为驱动目标：编辑器下一帧实际到位即与基线一致，不再被误判
                // 成「编辑器用户主动滚动」而把预览拽回（来回跳动）；预览现为主导，旧等待
                // 目标作废。
                state.driven_editor_target = Some(pb);
                state.driven_editor_await = 0;
                state.editor_block = Some(pb);
            } else {
                shell_core::rp_diag::diag("sync_guard_blocked", "site=preview_follow");
            }
        }
    } else if state.driven_preview_target.is_none()
        // 预览已稳定（上帧到本帧未变）：
        && prev.preview_block == preview_block
    {
        if state.driven_editor_target.is_some() {
            // 编辑器落位标记悬挂在「两侧静态未对齐」态（如大跳过冲）：推进宽限计数，
            // 超限清标（不重钉预览）后在下一帧走收敛。
            if state.driven_editor_await >= DRIVEN_EDITOR_MAX_AWAIT {
                state.driven_editor_target = None;
                state.driven_editor_await = 0;
            } else {
                state.driven_editor_await += 1;
            }
        } else if let Some(pb) = preview_block
            && Some(pb) != editor_block
        {
            // 以（用户当前所在的）静态预览为准回拉编辑器：幂等收敛「大跳后编辑器过冲
            // 未对齐」，不再依赖原「编辑器→预览」回声把预览拽回（那正是来回跳动/滚不
            // 下去的根因）。md 恢复在途例外（与上方「预览自主跨块跟随」同守卫）：恢复期
            // 「预览稳定在顶 + 编辑器在恢复行」是装配还没跟上，不是用户把编辑器移走。
            if tab.payload.md_restore_pending.is_none() {
                editor.update(cx, |ed, _| ed.scroll_to_line(pb));
                state.driven_editor_target = Some(pb);
                state.driven_editor_await = 0;
                state.editor_block = Some(pb);
            } else {
                shell_core::rp_diag::diag("sync_guard_blocked", "site=static_pull");
            }
        }
    }
}
