//! SC-4（2026-11-10）：`impl AppHost for EditorApp`——宿主服务面的 shell 侧实现。
//!
//! 借用纪律（见 `shell_core::host` 模块注释）：`AppHost` 方法一律 `&self`、实现**不得
//! 同步**读/写 `AppModel`（动作方法运行时宿主正持 `AppModel` 可变借用）；凡回调体需读
//! 模型（订阅 / 延迟），把模型读写放进**回调体**再经 `own_weak` 再入宿主实体——此刻
//! 动作方法的模型借用已结束。纯取值方法（`is_detached` 等）只读宿主字段，安全。

use super::*;
use shell_core::AppHost;

impl AppHost for EditorApp {
    fn self_entity_id(&self) -> EntityId {
        self.own_entity_id
    }

    fn self_weak(&self) -> gpui::AnyWeakEntity {
        self.own_weak.clone().into()
    }

    fn notify_host(&self, cx: &mut App) {
        cx.notify(self.own_entity_id);
    }

    fn is_constructing(&self) -> bool {
        self.constructing
    }

    fn is_detached(&self) -> bool {
        self.is_detached
    }

    fn is_session_persist(&self) -> bool {
        self.session_persist
    }

    fn root_focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn subscribe_editor(
        &self,
        editor: &gpui::Entity<editor::CodeEditor>,
        cx: &mut App,
    ) -> Subscription {
        let weak = self.own_weak.clone();
        let ed = editor.clone();
        cx.subscribe(&ed, move |emitter, ev: &editor::EditorEvent, cx| {
            // 事件回调触发时动作方法的模型借用早已结束：经自身弱句柄再入宿主实体，
            // 执行与既有 `subscribe_editor` 逐字一致的订阅体（行为/时机零变化）。
            let _ = weak.update(cx, |this, cx| {
                let pos = this.model.borrow().tabs.iter().position(|t| t.editor == emitter);
                if let Some(t) = pos {
                    match ev {
                        editor::EditorEvent::Edited => this.on_editor_edited(t, cx),
                        // 纯光标/选区移动（鼠标点击/方向键/搜索跳转）不走重活，
                        // 只刷新选项卡缓存的行/列 → 状态栏 `Ln x, Col y` 即时跟随。
                        editor::EditorEvent::SelectionChanged => {
                            this.on_editor_selection_changed(t, cx)
                        }
                        _ => {}
                    }
                }
            });
        })
    }

    fn files_root(&self) -> Option<std::path::PathBuf> {
        self.sidebar.files.root.clone()
    }

    fn sidebar_width_manual(&self) -> Option<f32> {
        if self.sidebar.width_auto {
            None
        } else {
            Some(self.sidebar.width)
        }
    }

    fn is_test_build(&self) -> bool {
        cfg!(test)
    }
}
