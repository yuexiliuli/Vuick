//! 跨窗口选项卡搬迁与拖拽：**域内纯逻辑** + `TabDragHost` 宿主服务面（2026-11-11 自
//! shell `app/tab_drag.rs` / `app/tabs.rs` 迁入并解耦，同 `lib.rs` 模块清单的 H2 拖出补全）。
//!
//! 拆分层：
//!
//! - **纯计算**（本模块函数，`shell_tabs::drag`）：拖拽预告尺寸兜底估算 [`tab_preview_size`]、
//!   窗内拖拽 picture 将被窗口边缘遮断判定 [`drag_picture_about_to_clip`]、栏内排序命中
//!   （[`tab_insert_index`] / [`tab_drag_over_row`]）、幽灵窗布局参数（[`tab_drag_ghost_layout`]）、
//!   拖拽状态机单步迁移（[`tab_drag_move_step`]）与拆分/还原决策（[`tab_drag_release_kind`]）。
//!   全部**不依赖宿主类型**（只读 `&Window` / 共享 `AppModel` / 宿主 `tab_bounds` 快照
//!   以参数传入），可独立编译 / 独立测试。
//! - **宿主服务面**（[`TabDragHost`]）：拖拽流程对宿主的副作用收敛为最小接口——开/建窗
//!   （`open_window` 建新窗、幽灵窗）、窗口注册表查询/登记、HWND（`SetWindowPos` / DWM），
//!   以及幽灵窗实体创建/销毁、adopt/restore 目标窗操作。宿主 `EditorApp` 在 shell
//!   `app/host_impl.rs` `impl TabDragHost for EditorApp`（方法体自原 `app/tab_drag.rs` 既有
//!   方法薄转发，行为零变化）。
//!
//! ## 借用纪律
//!
//! 纯计算函数持 `&RefCell<AppModel>` 即跨帧共享模型（与 [`crate::bar::TabBar`] 同一姿势）；
//! 继承 `TabsHost` 先例：**事件回调内经 `WeakEntity<H>::update` 反调宿主**，渲染期（宿主
//! 正持 `&mut EditorApp`）禁入宿主。拖拽移动/松开编排（`update_drag_ghost` /
//! `release_tab_drag_outside`）在宿主 layout.rs 回调体内以 `&mut EditorApp` 直调本宿主，
//! 无实体回租（不涉双借）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{px, Bounds, Context, Pixels, Point, SharedString, Size, Window};

use shell_core::{AppModel, DetachedTab};

/// 窗内拖拽 picture 距窗口边缘小于该值（px）即视为「将要被窗口边缘遮断」→ 提前切到
/// 同位置、同尺寸的完整版幽灵窗。余量保证：picture 真正开始被裁之前幽灵窗已接管，
/// 不存在「贴边被切 → 越界才突然完整」的可见跳变。
pub const DRAG_EDGE_CLIP_MARGIN: f32 = 4.0;

/// 选项卡栏拖拽命中边界槽（宿主 `tab_bounds` 元素类型；`TabBar` 实体渲染期每帧重建、
/// 记录器在 prepaint 写入真实 laid-out 边界）。
pub type TabBoundsCell = Rc<Cell<Bounds<Pixels>>>;

/// 拖拽预告（半透明选项卡）尺寸的**兜底估算**（仅真实 laid-out 尺寸尚未记录的首帧用）：
/// 宽 = `shape_line` 文本宽 + px_2 两侧(16) + 2×gap(8) + 关闭钮(18) + 边框(4) ≈ +46；
/// 高 = 选项卡 26。正常情况下 [`crate::bar::TabBar`] 的 `TabBoundsRecorder` + 尺寸记录器
/// 已把真实 laid-out 尺寸写入，不会走这里。
pub fn tab_preview_size(name: &str, dirty: bool, window: &Window) -> Size<Pixels> {
    let text = if dirty {
        format!("● {name}")
    } else {
        name.to_string()
    };
    let line = window
        .text_system()
        .shape_line(SharedString::from(text), px(12.), &[], None);
    let text_w = line.width();
    gpui::size((text_w + px(46.)).max(px(64.)), px(26.))
}

/// 窗内 gpui 拖拽 picture 是否**将要**被窗口边缘遮断（含 [`DRAG_EDGE_CLIP_MARGIN`] 余量）？
/// 拖拽 picture 恒画在 `鼠标 - 抓取偏移`（gpui window.rs `prepaint_as_root(mouse - cursor_offset)`），
/// 原点为窗口坐标、越出窗口帧缓冲的部分被窗口边缘裁掉——而光标本身仍可留在窗内（如抓取
/// 偏移偏大、picture 已探出边缘）。`window_size` 用 `window.bounds().size`（与 `on_drag_move`
/// 判 `inside` 同一坐标空间）。返回 true → 宿主 `update_drag_ghost` 提前显示**同位置、
/// 同尺寸**的完整幽灵窗（屏幕原点 = 窗左上角 + (pos − grab)，与窗内 picture 同位 → 无跳变）。
pub fn drag_picture_about_to_clip(
    pos: Point<Pixels>,
    grab: Point<Pixels>,
    size: Size<Pixels>,
    window_size: Size<Pixels>,
) -> bool {
    let m = px(DRAG_EDGE_CLIP_MARGIN);
    let o = pos - grab; // picture 左上角（窗口坐标）
    o.x < m
        || o.y < m
        || o.x + size.width > window_size.width - m
        || o.y + size.height > window_size.height - m
}

/// 计算拖拽光标 x 落在选项卡行内的「插入下标」（相对当前 `tabs`；`tabs.len()` = 插到末尾）：
/// - 最左 / 最右 / 选项卡间空隙 → 对应插入位
/// - 落在某选项卡内 → 左半 = 该选项卡之前（下标 = 该选项卡），右半 = 之后（下标 +1）
/// 「是否已入行」由调用方 [`tab_drag_over_row`] 负责（被拖选项卡边缘进入行即算），本函数
/// 只按 x 定下标。命中依据宿主 `tab_bounds`（上一帧渲染的真实 laid-out 边界，与输入坐标
/// 同系、含横向滚动；由 `TabBar` 实体渲染期写回宿主字段）——以参数传入即纯计算。
pub fn tab_insert_index(
    x: Pixels,
    tab_bounds: &[TabBoundsCell],
    tabs_len: usize,
) -> Option<usize> {
    if tabs_len == 0 || tab_bounds.is_empty() {
        return None;
    }
    let first = tab_bounds[0].get();
    if first.size.width <= px(0.) {
        return None; // 尚未渲染出真实边界（首帧）
    }
    if x < first.origin.x {
        return Some(0);
    }
    for (i, cell) in tab_bounds.iter().enumerate() {
        let b = cell.get();
        if x < b.origin.x {
            return Some(i); // 落在选项卡 i 左侧的空隙
        }
        if x <= b.bottom_right().x {
            let mid = b.origin.x + b.size.width / 2.;
            return Some(if x < mid { i } else { i + 1 });
        }
    }
    Some(tab_bounds.len())
}

/// 拖拽中的选项卡（picture 盒 `drag`，= `鼠标 - 抓取偏移` 处、按真实 tab 尺寸）的
/// 上下边缘是否**进入**选项卡行内的 y 区间（任一边进入即 true）：白线出现的触发——
/// **不要求鼠标光标在行内**，只要被拖选项卡的边缘碰到行即可。
/// 命中依据宿主 `tab_bounds`（含横向滚动；以参数传入即纯计算）。
pub fn tab_drag_over_row(drag: Bounds<Pixels>, tab_bounds: &[TabBoundsCell]) -> bool {
    if tab_bounds.is_empty() {
        return false;
    }
    let first = tab_bounds[0].get();
    if first.size.width <= px(0.) {
        return false; // 尚未渲染出真实边界（首帧）
    }
    let row_top = first.origin.y;
    let row_bottom = first.bottom_right().y;
    drag.origin.y < row_bottom && (drag.origin.y + drag.size.height) > row_top
}

/// 幽灵窗布局参数（纯计算，宿主 `update_drag_ghost` 消费）：屏幕原点 + 是否显示。
#[derive(Clone, Copy, Debug)]
pub struct DragGhostLayout {
    /// 幽灵左上角（屏幕逻辑 px）＝ 源窗口左上角 + (拖拽点 − 抓取偏移)，与 gpui 在
    /// 窗内绘制拖拽 picture 的位置（mouse_position - cursor_offset）完全一致。
    pub screen: Point<Pixels>,
    /// 幽灵窗显示条件：光标越出窗口（窗外完整版）**或** 窗内 picture 将要被窗口边缘
    /// 遮断（含 [`DRAG_EDGE_CLIP_MARGIN`] 余量，见 [`drag_picture_about_to_clip`]）——
    /// 贴边即提前切同位置同尺寸的完整幽灵窗，杜绝「一半被裁 → 越界才突然完整」。
    pub visible: bool,
}

/// 幽灵窗布局参数（纯计算）：屏幕原点 = 窗左上角 + (拖拽点 − 抓取偏移)（与窗内 picture
/// 同位 → 无跳变）；显示条件 = 越出窗口 **或** 窗内 picture 将要被窗口边缘遮断。
/// 实际建窗 / `SetWindowPos` 定位 / 显隐经 [`TabDragHost`] 宿主执行。
pub fn tab_drag_ghost_layout(
    window_origin: Point<Pixels>,
    window_size: Size<Pixels>,
    pos: Point<Pixels>,
    grab: Point<Pixels>,
    size: Size<Pixels>,
    outside: bool,
) -> DragGhostLayout {
    let screen = window_origin + (pos - grab);
    let visible = outside || drag_picture_about_to_clip(pos, grab, size, window_size);
    DragGhostLayout { screen, visible }
}

/// 拖拽状态机单步迁移（`on_drag_move` 期间，纯模型 + 命中计算）的结果。宿主 layout.rs
/// 在 `on_drag_move` 回调内调用 [`tab_drag_move_step`]，据返回的 `outside` 继续维护幽灵窗。
#[derive(Clone, Copy, Debug)]
pub struct DragMoveStep {
    /// 光标是否越出本窗口（=「松开才建窗」预位）。
    pub outside: bool,
    /// 本次移动算出的「栏内排序插入下标」（越窗 / 盒未入行 = None）。
    pub reorder_target: Option<usize>,
}

/// 拖拽状态机（`on_drag_move`）单步迁移：写共享模型的拖拽态（`tab_dragging` /
/// `drag_outside_armed` / `tab_reorder_target`），返回本次移动的 `outside` 与排序命中——
/// 「拖拽状态迁移」（含读 `AppModel` 态）全部收在域内，宿主只消费返回值。下标越界
/// （已拔出后残留移动事件）→ `None`，宿主早退（与原 `on_drag_move` 守卫一致）。
pub fn tab_drag_move_step(
    model: &RefCell<AppModel>,
    tab_bounds: &[TabBoundsCell],
    idx: usize,
    pos: Point<Pixels>,
    grab: Point<Pixels>,
    drag_size: Size<Pixels>,
    window_bounds: Bounds<Pixels>,
) -> Option<DragMoveStep> {
    if idx >= model.borrow().tabs.len() {
        return None;
    }
    model.borrow_mut().tab_dragging = Some(idx);
    let inside = Bounds::new(Point::default(), window_bounds.size).contains(&pos);
    let outside = !inside;
    model.borrow_mut().drag_outside_armed = outside;
    // 栏内排序插入位：以**被拖选项卡 picture 盒的边缘**是否进入选项卡行为准
    // （盒上/下任一边碰到行即激活白线）；插入下标按光标 x 判定。拖出窗口 / 盒未入行
    // → None（不排序 / 拆窗）。
    let drag_bounds = Bounds::new(pos - grab, drag_size);
    let reorder_target = if inside && tab_drag_over_row(drag_bounds, tab_bounds) {
        tab_insert_index(pos.x, tab_bounds, model.borrow().tabs.len())
    } else {
        None
    };
    model.borrow_mut().tab_reorder_target = reorder_target;
    Some(DragMoveStep {
        outside,
        reorder_target,
    })
}

/// 窗外松开的目标窗口决策（纯判定，读宿主 `is_detached`）：
/// - 主窗口拖出 → 拆进新开的独立窗口（窗口出现在鼠标释放处）；
/// - 独立（detached）窗口拖出 → 还原回主窗口（找不到主窗口则自我转正为主窗口）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragRelease {
    SplitToNewWindow,
    RestoreToPrimary,
}

/// 拆分决策（纯）：按 `is_detached` 判定窗外松开的动作——主窗口 → 拆新窗、独立窗口 →
/// 还原主窗。「真正执行」（`open_window` 建新窗 / 注册表查询 / `remove_window`）经
/// [`TabDragHost::tab_drag_host_release_outside`] 宿主分发。
pub fn tab_drag_release_kind(is_detached: bool) -> DragRelease {
    if is_detached {
        DragRelease::RestoreToPrimary
    } else {
        DragRelease::SplitToNewWindow
    }
}

/// 跨窗口选项卡拖拽的**宿主服务面**：把拖拽流程对宿主的副作用收敛为最小接口，使本
/// crate 不依赖 shell 的具体宿主类型。shell 侧 `impl TabDragHost for EditorApp` 在
/// `app/host_impl.rs`（方法体自原 `app/tab_drag.rs` 既有方法薄转发，行为零变化）。
/// 拖拽开始预建幽灵窗（原 `TabsHost::tabs_host_prime_drag_ghost`，H1 期间放在 TabsHost，
/// H2 随其余拖拽宿主入口一起迁此）。
pub trait TabDragHost: 'static {
    /// 拖拽开始预建窗外幽灵窗（原 `EditorApp::prime_drag_ghost`）：把 Windows 透明置顶
    /// popup 的 OS 创建成本（首帧 DCOMP）前移到拖拽起点。宿主独占：`Context<Self>` 建
    /// `TabDragGhost` 幽灵窗实体 + 宿主 `drag_ghost`/`drag_ghost_pending` 字段。
    fn tab_drag_host_prime_ghost(
        &mut self,
        idx: usize,
        name: SharedString,
        dirty: bool,
        size_cell: Rc<Cell<Size<Pixels>>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 拖拽移动期间维护幽灵窗（原 `EditorApp::update_drag_ghost`）：取 tab 元数据 →
    /// 域 [`tab_drag_ghost_layout`] 算布局参数 → 懒建/复用幽灵窗 + `SetWindowPos` 定位 +
    /// 写 `drag_ghost_visible`。宿主独占：建窗 / HWND / `drag_ghost` 句柄字段 + win32 定位。
    fn tab_drag_host_update_ghost(
        &mut self,
        idx: usize,
        drag_size: Rc<Cell<Size<Pixels>>>,
        pos: Point<Pixels>,
        outside: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 销毁跟随鼠标的幽灵窗（回到窗内 / 松开 / 取消拖拽时；幂等）。宿主独占：
    /// `drag_ghost` 句柄字段 + `remove_window` 窗口销毁副作用。
    fn tab_drag_host_destroy_ghost(&mut self, cx: &mut Context<Self>)
    where
        Self: Sized;

    /// 取模型载荷（原 `EditorApp::take_tab_payload`，动作体已迁 `AppModel`）：整结构 move
    /// `EditorTab` → `DetachedTab`。经宿主暴露维持既有 `pub` 路径（`tests/pdf.rs` 多窗口
    /// 换位回归 / 宿主 tabs.rs 复制新窗直调）。
    fn tab_drag_host_take_payload(&mut self, idx: usize) -> Option<DetachedTab>
    where
        Self: Sized;

    /// live2 守卫后取载荷（原 `EditorApp::commit_live_and_take_tab_payload`）：实时预览编辑
    /// 实体不随选项卡搬迁——先 `commit_live` 把 live2 写回编辑区再取载荷。宿主独占：
    /// `commit_live` 的宿主实体/订阅 + WYSIWYG 回收副作用。
    fn tab_drag_host_commit_live_and_take_payload(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DetachedTab>
    where
        Self: Sized;

    /// 窗外松开编排（原 `EditorApp::release_tab_drag_outside`）：销毁幽灵 → 取载荷 →
    /// 按 [`tab_drag_release_kind`]（读宿主 `is_detached`）拆新窗 / 还原主窗 → 本窗口销毁。
    /// 宿主独占：`open_window` 建新窗 / `EditorAppRegistry` 查询 / `remove_window`。
    fn tab_drag_host_release_outside(
        &mut self,
        window: &mut Window,
        release_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 把选项卡拆进新开独立窗口（原 `EditorApp::split_tab_to_new_window`）：`open_window`
    /// 建窗（构造 `new_detached`），窗口左上角跟随鼠标释放位置。宿主独占：`open_window`
    /// + `EditorApp::new_detached` 构造（注册表登记随窗口创建发生）。
    fn tab_drag_host_split_to_new_window(
        &mut self,
        payload: DetachedTab,
        release_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;

    /// 还原到主（非 detached）窗口（原 `EditorApp::restore_tab_to_primary`）：`EditorAppRegistry`
    /// 查询首个非 detached 窗口 → 目标窗 `adopt_tab` 收纳 + 聚焦；找不到主窗口则本窗转正
    /// 原地收纳。宿主独占：注册表查询 / 跨窗口实体 update / 焦点。
    fn tab_drag_host_restore_to_primary(
        &mut self,
        payload: DetachedTab,
        cx: &mut Context<Self>,
    ) where
        Self: Sized;
}
