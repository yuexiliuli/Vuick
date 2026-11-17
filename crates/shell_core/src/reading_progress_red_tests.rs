//! 阅读进度两缺陷的「复现红测」（2026-11-17 审查 `docs/code-review-2026-11-17/26-shell_core.md`
//! 的 M 两项）。只写失败断言测试，**不改任何生产源码**。工作树含并行 agent 在途改动，
//! 本模块只新增测试文件 + lib.rs 一个 `#[cfg(test)]` 挂载 + Cargo dev-dependency。
//!
//! ## 缺陷 1（拆窗阅读进度不落盘，actions.rs:413-420）
//! `save_reading_progress` 以 `!is_session_persist() || is_detached()` 早退。detached 窗口
//! （`shell init.rs`：`new_detached` = `new_impl(..., true /*is_detached*/, false /*startup*/)`，
//! 即 `is_detached=true` 且 `session_persist=false`）的阅读进度永不写盘。但 reading_progress
//! 是**文件级**全局存储（按文件路径做键，多窗口无冲突），不应被「会话责任窗口」门控跳过。
//! 期望语义：detached 窗口的进度照常写入。→ 红测：构造 detached 上下文驱动 `save_reading_progress`，
//! 断言写入发生（当前早退 → 未写 = 红）。对照分支：同一模型主窗口宿主保存 → 绿（证明判定面正确）。
//!
//! ## 缺陷 2（live_edit 进度读隐藏源码区顶行，actions.rs:387-405）
//! `tab_reading_position` 对 md/txt 选项卡一律读 `payload.editor.viewport_top_line()`（源码编辑区）。
//! live_edit 期间用户实际在 live2（WYSIWYG 编辑面）滚动阅读，源码编辑区被隐藏、顶行停在进入
//! live 时的陈旧值 → 进度存到陈旧顶行。期望语义：live_edit 进度以 live2 的 `top_source_line()`
//! 为准。→ 红测：构造「源码编辑区顶行(0) ≠ live2 当前进度(深行)」的 live_edit 选项卡，断言
//! 进度 == live2 的值（当前取源码区顶行 → 不一致 = 红）。
//!
//! ## 可测性
//! - 缺陷 1：完全 headless 可测（`#[gpui::test]` + 假 host，内存 map 承担「已写入」可观测量，
//!   `is_test_build=true` 短路磁盘写）。
//! - 缺陷 2：headless 可测，需真实渲染 live2 一次以建 frame + 滚动（镜像 markdown_wysiwyg
//!   既有 `#[gpui::test]` 姿势：`add_window_view` 渲染 WyEditor → `set_host_scroll` →
//!   `scroll_to_line` → `set_viewport_top` → `top_source_line`）。

use std::path::{Path, PathBuf};

use gpui::{App, AppContext, Context, Entity, FocusHandle, IntoElement, Render, ScrollHandle, Subscription, TestAppContext, VisualTestContext, Window};
use markdown_wysiwyg::WyEditor;
use session_store::{ReadingProgress, reading_progress};

use crate::host::AppHost;
use crate::model::AppModel;

/// 假宿主：只读标志 + 订阅，满足 `AppHost` 最小面（`is_test_build=true` → 测试不落盘，
/// 内存 map 承担观测量）。`detached/session_persist` 镜像 `shell init.rs` 各窗口族的真实取值。
struct TestHost {
    entity_id: gpui::EntityId,
    weak: gpui::AnyWeakEntity,
    detached: bool,
    session_persist: bool,
}

impl AppHost for TestHost {
    fn self_entity_id(&self) -> gpui::EntityId {
        self.entity_id
    }
    fn self_weak(&self) -> gpui::AnyWeakEntity {
        self.weak.clone()
    }
    fn notify_host(&self, _cx: &mut App) {}
    fn is_constructing(&self) -> bool {
        false
    }
    fn is_detached(&self) -> bool {
        self.detached
    }
    fn is_session_persist(&self) -> bool {
        self.session_persist
    }
    fn root_focus_handle(&self, cx: &App) -> FocusHandle {
        cx.focus_handle()
    }
    fn subscribe_editor(
        &self,
        editor: &Entity<editor::CodeEditor>,
        cx: &mut App,
    ) -> Subscription {
        // 测试只驱动纯保存/读取面，事件订阅空实现即可。
        cx.subscribe(editor, |_, _: &editor::EditorEvent, _| {})
    }
    fn files_root(&self) -> Option<PathBuf> {
        None
    }
    fn sidebar_width_manual(&self) -> Option<f32> {
        None
    }
    fn is_test_build(&self) -> bool {
        true
    }
}

/// 渲染 WyEditor（live2）的窗口宿主：建一次 frame 供 `top_source_line` 使用。
struct WyHost {
    wy: Entity<WyEditor>,
}

impl Render for WyHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.wy.clone()
    }
}

/// 假实体：给 `TestHost` 提供 `entity_id` / `AnyWeakEntity`。
struct Marker;

/// 渲染若干帧（镜像 editor/wysiwyg 既有测试的收敛姿势）。
fn settle(cx: &mut VisualTestContext) {
    for _ in 0..4 {
        cx.run_until_parked();
        let _ = cx.refresh();
    }
    cx.run_until_parked();
}

/// 便捷：带文件选项卡的默认模型 + 主窗口语义宿主。
/// 返回 (model, idx, path)。
fn model_with_file_tab(
    cx: &mut TestAppContext,
    host: &TestHost,
    path: &Path,
) -> (AppModel, usize) {
    let mut model = AppModel::default();
    let idx = cx.update(|cx| model.make_tab(host, cx));
    cx.update(|_cx| {
        model.tabs[idx].payload.file_path = Some(path.to_path_buf());
    });
    (model, idx)
}

/// RED 1：detached 窗口（`is_detached=true` + `session_persist=false`，镜像 `new_detached`）
/// 的阅读进度照常写入文件级 map——当前门控早退 → 未写 = 红。
#[gpui::test]
fn detached_window_reading_progress_still_persists(cx: &mut TestAppContext) {
    let marker = cx.new(|_| Marker);
    let path = std::env::temp_dir().join(format!("gpui-rp-detached-red-{}.md", std::process::id()));

    let main_host = TestHost {
        entity_id: marker.entity_id(),
        weak: marker.downgrade().into(),
        detached: false,
        session_persist: true,
    };
    let detached_host = TestHost {
        entity_id: marker.entity_id(),
        weak: marker.downgrade().into(),
        detached: true,
        session_persist: false,
    };
    let key = reading_progress::normalize_key(&path);

    // —— 对照分支：主窗口宿主保存 → 应写入（证明判定面 / 驱动路径正确）——
    {
        let (mut model, idx) = model_with_file_tab(cx, &main_host, &path);
        cx.update(|cx| model.save_reading_progress(&main_host, idx, cx));
        let saved = cx.update(|_cx| model.reading_progress.contains_key(&key));
        assert!(saved, "脚手架对照：主窗口（非 detached）应已把进度写入文件级 map");
    }

    // —— RED 分支：detached 窗口保存 → 也应变写入（阅读进度是文件级，不随窗口 detached 跳过）——
    let (mut model, idx) = model_with_file_tab(cx, &detached_host, &path);
    cx.update(|cx| model.save_reading_progress(&detached_host, idx, cx));
    let saved_detached = cx.update(|_cx| model.reading_progress.contains_key(&key));
    assert!(
        saved_detached,
        "RED: detached 窗口阅读进度也应写入文件级 map（当前 is_detached 提前 return → 未写）"
    );
}

/// RED 2：live_edit（WYSIWYG）期间阅读进度以 live2 视口顶行（`top_source_line`）为准，
/// 而非隐藏源码编辑区顶行——当前 `tab_reading_position` 读 `payload.editor.viewport_top_line()`
/// → 构造「源码区顶行(0) ≠ live2 进度(深行)」断言进度 == live2 → 不一致 = 红。
#[gpui::test]
fn live_edit_reading_progress_reads_live2_not_source(cx: &mut TestAppContext) {
    // live2 源文本：每行一个独立标题块（`### 第 i 节` + 空行）。块起点 = 偶数源码行。
    let mut live2_text = String::new();
    for i in 0..60 {
        live2_text.push_str(&format!("### 第 {i} 节\n\n"));
    }
    const LIVE2_TARGET: usize = 80; // 块 i=40（起始源码行 80 = 偶数块起点），深读目标行。

    // —— 构造 + 滚动 live2（真实渲染建 frame；镜像 wysiwyg 既有 `#[gpui::test]` 姿势）——
    let holder: std::rc::Rc<std::cell::RefCell<Option<Entity<WyEditor>>>> = std::rc::Rc::default();
    let (_wy_host, mut vcx_wy) = cx.add_window_view(|window, cx| {
        let wy = cx.new(|cx| WyEditor::new(window, cx).with_text(&live2_text));
        *holder.borrow_mut() = Some(wy.clone());
        WyHost { wy }
    });
    let live2 = holder.borrow().clone().expect("live2 实体应已创建");
    settle(&mut vcx_wy);
    let vhand = ScrollHandle::new();
    // 注入宿主滚动句柄（scoll_to_line_impl 写帧 + 偏移；镜像宿主每帧注入姿势）。
    live2.update(vcx_wy, |w, _| w.set_host_scroll(Some(vhand.clone()), 0.0));
    vcx_wy.update(|window, cx| {
        // 滚动到深行（远端目标块：建帧 + 写宿主偏移）。
        live2.update(cx, |w, _| w.scroll_to_line(LIVE2_TARGET, window));
    });
    let live2_top = vcx_wy.update(|_w, cx| {
        // live2 宿主每帧推入「视口顶 px」（= −宿主滚动偏移）；当前停在目标块顶。
        live2.update(cx, |w, _| w.set_viewport_top((-vhand.offset().y.as_f32()).max(0.)));
        live2.read(cx).top_source_line()
    });
    // 脚手架自证：live2 可真滚到深行（frame 已建、滚动生效），不是 0（顶）。
    assert!(
        live2_top.is_some() && live2_top.unwrap() > 0,
        "脚手架：live2 视口顶源码行应可读且在深行，当前={live2_top:?}"
    );
    let live2_top = live2_top.expect("live2 视口顶应已可读");

    // —— 组装 AppModel + live_edit 选项卡 ——
    let marker = cx.new(|_| Marker);
    let host = TestHost {
        entity_id: marker.entity_id(),
        weak: marker.downgrade().into(),
        detached: false,
        session_persist: true,
    };
    let path = std::env::temp_dir().join(format!("gpui-rp-live2-red-{}.md", std::process::id()));
    let (mut model, idx) = model_with_file_tab(cx, &host, &path);
    cx.update(|_cx| {
        let t = &mut model.tabs[idx];
        t.live_edit = true;
        t.live2 = Some(live2.clone());
        // 源码编辑区保持默认（未渲染/未滚动 → viewport_top_line == 0）＝ live 期间隐蔽不动的陈旧位置。
    });

    // 场景前提：源码编辑区顶行 ≠ live2 当前进度（深行）。
    let src_top = cx.update(|cx| model.tabs[idx].payload.editor.read(cx).viewport_top_line());
    assert_ne!(
        src_top, live2_top,
        "脚手架：需构造「源码区顶行 ≠ live2 进度」场景（src_top={src_top}, live2_top={live2_top}）"
    );

    let got = cx.update(|cx| model.tab_reading_position(idx, &*cx));
    assert_eq!(
        got,
        Some(ReadingProgress::Markdown { top_line: live2_top }),
        "RED: live_edit 期间阅读进度应以 live2 视口顶行 {live2_top} 为准（当前取隐藏源码编辑区顶行 {src_top}）"
    );
}
