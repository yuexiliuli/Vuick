//! AppHost——宿主服务面（SC-4，2026-11-10 自 shell 的 `impl EditorApp` 动作方法体迁出）。
//!
//! 迁入 `crates/shell_core::AppModel` 的动作方法体（tabs 动作半 / tab_drag payload 半 /
//! session 编排纯逻辑 / file_ops 纯逻辑）要把原本对宿主的直调统一收口到本 trait：
//! AppModel 方法持 `&dyn AppHost`（宿主把 `&*self` 作为 `&dyn AppHost` 传进来），
//! 宿主服务在本 trait 声明、在 `impl AppHost for EditorApp`（shell 侧）实现。
//!
//! ## 借用纪律（本 trait 方法的硬约束，违反 = 运行期 RefCell 双借 panic）
//!
//! 动作方法被调用时，宿主正持有 `AppModel` 的**可变借用**（
//! `self.model.borrow_mut().yyy(host_self, …)`）；因此：
//!
//! - 本 trait 的所有方法都是 `&self`，且**实现不得同步读/写 `AppModel`**——模型态只由
//!   `AppModel` 方法本体直接访问（同 crate 字段直取），host 回读模型即双借。
//! - 需在**异步 / 延迟回调**里触碰模型的：把模型读写放进回调体（此刻模型守卫已释放），
//!   或经 `self_weak()` 再入宿主实体执行后、在实体回调体里读模型（同样安全：事件回调
//!   触发时动作方法的模型借用早已结束）。
//! - 只读宿主字段的纯取值方法（`is_detached` / `is_session_persist` 等）无此约束。

use std::path::PathBuf;

use gpui::{App, AnyWeakEntity, Entity, EntityId, FocusHandle, Subscription};

/// 选项卡编辑区实体（宿主订阅对象；shell_core 依赖 editor，可命名）。
use editor::CodeEditor;

/// 宿主服务面。实现方是宿主的 `EditorApp`（shell），本 crate 只声明接口。
pub trait AppHost: 'static {
    /// 宿主实体自身 id：动作方法里 `cx.notify()` 的落点
    /// （`Context::notify()` 内部即 `app.notify(entity_id)`）。
    fn self_entity_id(&self) -> EntityId;

    /// 宿主实体自身的弱句柄（擦除类型）：异步任务 / 延迟回调经它再入宿主实体。
    /// 返回 `AnyWeakEntity` 是为保证本 crate 无需命名宿主具体类型（如 `EditorApp`）。
    fn self_weak(&self) -> AnyWeakEntity;

    /// 宿主通知（= 原 `Context::notify()`：标脏宿主实体所在窗口，下一帧重绘）。
    fn notify_host(&self, cx: &mut App);

    /// 拆出/拖拽窗口构造期标志（宿主 `constructing` 字段）：构造中抑制多余 notify，
    /// 避免新窗口白画一整帧冗余重绘。
    fn is_constructing(&self) -> bool;

    /// 是否独立文件窗口（拖出拆分生成；宿主 `is_detached`）。
    fn is_detached(&self) -> bool;

    /// 会话持久化责任（仅首个主窗口 `session_persist` 为 true；媒体/拆出窗口 false）。
    fn is_session_persist(&self) -> bool;

    /// App 根焦点句柄（零选项卡 / 只读预览无编辑区时聚焦到窗口级快捷键可用的根）。
    fn root_focus_handle(&self, cx: &App) -> FocusHandle;

    /// 订阅编辑器事件（宿主实现：经自身弱句柄把回调挂到宿主实体，回调体才读模型）。
    fn subscribe_editor(&self, editor: &Entity<CodeEditor>, cx: &mut App) -> Subscription;

    /// 文件树当前根路径（宿主 `sidebar.files.root`；prefs 持久化 `opened_folder` 用）。
    /// 纯取值，只读宿主字段。
    fn files_root(&self) -> Option<PathBuf>;

    /// 侧栏宽度（宿主 `sidebar.width_auto` —— 未手动拖过 = None，不持久化）。
    /// 纯取值，只读宿主字段。
    fn sidebar_width_manual(&self) -> Option<f32>;

    /// 当前是否为 **shell 测试构建**（宿主侧 `cfg!(test)`）。
    ///
    /// 迁入 `AppModel` 的持久化逻辑（`persist_session` / `persist_prefs`）原以
    /// `cfg!(test)` 在 shell 侧短路「测试不落盘」；shell_core 作依赖编译时该宏恒假，
    /// 无法复现。借此 getter 把「shell 是否测试构建」经宿主回传，行为逐字保持
    /// （测试构建恒跳过写盘；生产恒走盘）。
    fn is_test_build(&self) -> bool;
}
