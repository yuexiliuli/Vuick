//! 自绘下拉菜单：`PopupMenu`（实体）/ [`PopupMenuItem`] / [`Dropdown`]（触发宿主）+ [`DropdownMenu`] trait，
//! 替代 `gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem}`。
//!
//! # 结构
//! - [`DropdownMenu`]：trait，给 [`Button`](crate::ui::button::Button) 挂下拉菜单：
//!   `.dropdown_menu(|menu, _, _| …)` 返回 [`Dropdown`]（取代 gpui-component 的
//!   `DropdownMenuPopover`）。
//! - [`Dropdown`]：触发宿主。元素级状态（`use_keyed_state`，按宿主 id 隔离）持有
//!   `open` 与打开的 `Entity<PopupMenu>`；点击触发按钮（宿主 `on_mouse_down` 且按下点不在
//!   已开菜单内）切换开合；打开时用 `deferred(anchored(Local::TopLeft))` 在按钮左下挂菜单，
//!   并与菜单实体订阅 `DismissEvent`（菜单内选择/ Esc / 点击外关闭都会收敛到「关闭」）。
//! - [`PopupMenu`]：`Entity` + `Render`，持焦点、选中索引与渲染元素；键盘 ↑↓/Enter/Esc
//!   直接 `on_key_down` 处理（不引 keymap/actions）；点击外 `on_mouse_down_out` 关闭并
//!   把焦点还给开菜单前的句柄。
//!
//! # 范围
//! 提供单层 + 真悬浮二级（[`PopupMenuItem::flyout`]）菜单能力；`ElementItem` 自定义元素 +
//! `Separator`。滚动、kbd 绑定等 gpui-component 的重交互能力不迁移（App 未使用）。

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, AnimationExt, App, AppContext as _, Bounds, ClickEvent, Context,
    DismissEvent, Edges, ElementId, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Render, RenderOnce, SharedString, Size, StatefulInteractiveElement, Styled,
    Subscription, Window, accesskit, anchored, deferred, div, point, prelude::FluentBuilder as _,
    px, size,
};

use crate::ui::theme::Theme;

/// 菜单项点击处理器。
type ItemHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// 菜单项自定义内容构建器（每次渲染调用，保证读到最新应用状态）。
/// `Rc`（非 `Box`）：`PopupMenuItem` 实现 `Clone`——二级菜单交换（[`PopupMenuItem::submenu`]）
/// 需要把整组条目克隆进菜单（详见 `sub_stack`）。
type ItemRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement + 'static>;

/// 「标签 + debug_selector」菜单项工厂：右键/上下文菜单最常见的单一形态——
/// `div().w_full().flex().items_center().child(label).debug_selector(key)`。
/// 收敛 shell_tabs / shell_sidebar / shell / shell_media 各处手写的同构元素
/// （Pattern 1.2）；交互（`.on_click` / `.disabled` / `.keep_open` / `.flyout`…）
/// 在返回值上继续包装，标签/选择器键由调用方注入。
pub fn label_menu_item(
    label: impl Into<SharedString>,
    key: impl Into<String>,
) -> PopupMenuItem {
    let label: SharedString = label.into();
    let key = key.into();
    PopupMenuItem::element(move |_, _| {
        div()
            .w_full()
            .flex()
            .items_center()
            .child(label.clone())
            .debug_selector(|| key.clone())
    })
}

/// 单层下拉菜单项。
pub enum PopupMenuItem {
    /// 分隔线。
    Separator,
    /// 自定义元素 / 按钮式菜单项。
    ElementItem {
        render: ItemRenderer,
        disabled: bool,
        handler: Option<ItemHandler>,
        /// 选中/点击后**不关闭菜单**（保留焦点同实体换内容用，如「打开最近」More/返回）；
        /// 默认 false：保持既有「执行回调后关闭」语义。opt-in。
        keep_open: bool,
        /// 二级交换：激活该项时把菜单条目**原地**换成 `swap_to` 的一组（不关闭菜单，
        /// 「背景色 → 选项列表」用，**已被 [`PopupMenuItem::flyout`] 取代，dormancy 备用**）；
        /// 见 [`PopupMenuItem::submenu`]。
        swap_to: Option<Rc<Vec<PopupMenuItem>>>,
        /// 返回型：在二级列表中点击 → 换回上一级条目（菜单保持不关闭，**flyout 路径无返回项，
        /// dormancy 备用**）；见 [`PopupMenuItem::back`]。
        back: bool,
        /// 真悬浮二级菜单：悬停该项 → 次级菜单在父菜单**右侧**自动展开（父菜单其它项保持
        /// 可见，不原地替换、子列表无需「返回」项）。见 [`PopupMenuItem::flyout`]。
        flyout: Option<Rc<Vec<PopupMenuItem>>>,
    },
}

impl PopupMenuItem {
    /// 用一个自定义元素当菜单项内容（App 的 `menu_action_item` 用它排「标签 + 快捷键」）。
    pub fn element<F, E>(builder: F) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        PopupMenuItem::ElementItem {
            render: Rc::new(move |window, cx| builder(window, cx).into_any_element()),
            disabled: false,
            handler: None,
            keep_open: false,
            swap_to: None,
            back: false,
            flyout: None,
        }
    }

    /// 分隔线。
    pub fn separator() -> Self {
        PopupMenuItem::Separator
    }

    /// 注册点击回调（选中确认或指针点击时执行，随后关闭菜单）。
    pub fn on_click<F>(mut self, handler: F) -> Self
    where
        F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    {
        match &mut self {
            PopupMenuItem::ElementItem { handler: h, .. } => {
                *h = Some(Rc::new(handler));
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 禁用（置灰 + 不响应点击）。
    #[allow(dead_code)] // 公开 API：App 现未禁用菜单项，保留给后续调用点
    pub fn disabled(mut self, disabled: bool) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { disabled: d, .. } => {
                *d = disabled;
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 点击/选中后**不关闭菜单**（opt-in）：`PopupMenu::activate` 据此跳过 dismiss，
    /// 焦点保留（「打开最近…」用它保持文件菜单展开）；其余项不调用 = 既有行为不变。
    pub fn keep_open(mut self) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { keep_open, .. } => {
                *keep_open = true;
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 标记为「二级交换」项：激活时把菜单条目**原地**换成 `items` 这一组（不关闭菜单、
    /// 焦点保留）——「背景色 → 默认/夜间/护眼/羊皮纸/自定义/返回」二级菜单用。
    /// 子列表里的「返回」项用 [`Self::back`] 换回上一级；不想要返回型时子列表为普通条目。
    /// 实现走菜单内 `sub_stack` 记忆上一级，**不加真 hover 浮层**（单层面板原地换内容）。
    /// **Dormancy：已被 [`Self::flyout`] 真悬浮路径取代，暂留作备用。**
    pub fn submenu(mut self, items: Vec<PopupMenuItem>) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { swap_to, .. } => {
                *swap_to = Some(Rc::new(items));
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 标记为「返回」项：在二级列表中点击 → 换回上一级条目（菜单保持不关闭，「返回」用）。
    /// 仅在其所在列表是被 [`Self::submenu`] 换入时才有效（无上一级则忽略）。
    pub fn back(mut self) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { back, .. } => {
                *back = true;
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    /// 标记为「真悬浮二级菜单」项：悬停该项 → 次级菜单在父菜单**右侧**自动展开（父菜单
    /// 其它项保持可见，不原地替换、子列表无需「返回」项）。子列表普通条目点击语义与单项
    /// 一致（`keep_open` 等仍生效）。
    /// 与 [`Self::submenu`] 的原地交换是两条并列路径：submenu 保留作 dormancy（备用），
    /// 交互线默认走本方法。
    pub fn flyout(mut self, items: Vec<PopupMenuItem>) -> Self {
        match &mut self {
            PopupMenuItem::ElementItem { flyout, .. } => {
                *flyout = Some(Rc::new(items));
            }
            PopupMenuItem::Separator => {}
        }
        self
    }

    fn is_separator(&self) -> bool {
        matches!(self, PopupMenuItem::Separator)
    }
}

/// `PopupMenuItem` 克隆（渲染器/处理器/子菜单共享 `Rc`、浅克隆）：二级菜单交换
/// `sub_stack` 需要把「当前条目」与「子列表」各克隆一份进菜单；闭包本体共享不复制。
impl Clone for PopupMenuItem {
    fn clone(&self) -> Self {
        match self {
            PopupMenuItem::Separator => PopupMenuItem::Separator,
            PopupMenuItem::ElementItem {
                render,
                disabled,
                handler,
                keep_open,
                swap_to,
                back,
                flyout,
            } => PopupMenuItem::ElementItem {
                render: render.clone(),
                disabled: *disabled,
                handler: handler.clone(),
                keep_open: *keep_open,
                swap_to: swap_to.clone(),
                back: *back,
                flyout: flyout.clone(),
            },
        }
    }
}

/// 菜单项几何：常规项高。
const ITEM_HEIGHT: Pixels = px(26.);
/// 菜单项内边距。
const ITEM_PADDING: Pixels = px(8.);
/// 菜单面板内边距 + 项间距。
const PANEL_PADDING: Pixels = px(4.);
/// 菜单面板描边宽度（父/次级同款 `border_1`）：flyout 顶部对齐根项行顶需计入该项高度。
const MENU_BORDER: Pixels = px(1.);
/// 分割线行占高 = `h(px(1.))` + `my_1`（上下各 4px）+ 其后各项间 gap（`PANEL_PADDING`），
/// 与 `menu_item_row` 的 Separator 渲染一致。flyout 顶部对齐须按**真实行高**逐项累计：
/// 分割线行远矮于 `ITEM_HEIGHT`，若按「每项都是 26px」算，带分割线菜单（如 PDF 右键
/// 「背景色」在两条分隔线之后）会把次级菜单压到根行下方 17px×N。
const MENU_SEPARATOR_SLOT: Pixels = px(13.);
/// 菜单面板**最小宽**（`items_column` 的 `.min_w(px(160.))` 同值）：本 App 菜单项均短于该
/// 宽，故面板实际宽 = 该常量，flyout 边缘适配的宽度估算直接复用它（面板企业没做出更宽项）。
const PANEL_MIN_W: Pixels = px(160.);
/// 菜单边缘留白：父面板夹持（[`parent_panel_rect`]）与 shell 侧 `snap_to_window_with_margin`
/// 使用**同一值**，二者的面板顶/左估算才能逐像素一致（flyout 相对锚定不错位）。
pub const MENU_EDGE_MARGIN: Pixels = px(8.);

/// flyout 水平方向：`Right` = 父菜单右邻（现状）；`Left` = 翻到父菜单左邻（窗口右缘空间不足）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlyoutH {
    Right,
    Left,
}
/// flyout 垂直方向：`Down` = 从根项行向下（现状）；`Up` = 从根项行向上翻转（窗口底缘空间不足）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlyoutV {
    Down,
    Up,
}

/// 单行占位高（含其后项间 gap）：常规项 `ITEM_HEIGHT + PANEL_PADDING`；分隔线
/// `MENU_SEPARATOR_SLOT`。与 `row_top` 逐项累计一致。
fn row_slot(item: &PopupMenuItem) -> Pixels {
    if item.is_separator() {
        MENU_SEPARATOR_SLOT
    } else {
        ITEM_HEIGHT + PANEL_PADDING
    }
}

/// 渲染视图内的行（忽略尾部分隔线，与 `Render` 的过滤一致）。
fn snapshot_rows(rows: &[PopupMenuItem]) -> Vec<&PopupMenuItem> {
    let n = rows.len();
    rows.iter()
        .enumerate()
        .filter(|(ix, r)| !(*ix + 1 == n && r.is_separator()))
        .map(|(_, r)| r)
        .collect()
}

/// 面板高度估算：真实行高和 + (n-1)×项间 gap + 盒上下内边距 + 上下描边。全部基于同一组
/// 渲染常量，故对实际布局**逐像素精确**（父面板夹持与 shell snap 对齐的前提）。
fn panel_height(rows: &[PopupMenuItem]) -> Pixels {
    let rendered = snapshot_rows(rows);
    if rendered.is_empty() {
        return 2.0 * PANEL_PADDING + 2.0 * MENU_BORDER;
    }
    let real: Pixels = rendered
        .iter()
        .map(|r| {
            if r.is_separator() {
                MENU_SEPARATOR_SLOT - PANEL_PADDING
            } else {
                ITEM_HEIGHT
            }
        })
        .sum();
    real + (rendered.len() as f32 - 1.0) * PANEL_PADDING + 2.0 * PANEL_PADDING + 2.0 * MENU_BORDER
}

/// 面板宽度估算 = 最小宽（本 App 菜单无更宽项）。
fn panel_width() -> Pixels {
    PANEL_MIN_W
}

/// 父菜单面板窗口矩形（自算夹持，镜像 shell `anchored` 的`snap_to_window_with_margin`：
/// 先压底/右、再压顶/左，均以常量与窗口尺寸计算、**逐帧恒定**，不回流不读上一帧）。
/// flyout 相对此矩形做方向与锚定，二者同源 → 帧级一致、无振荡。
fn parent_panel_rect(
    parent_pos: Point<Pixels>,
    win: Size<Pixels>,
    parent_rows: &[PopupMenuItem],
) -> Bounds<Pixels> {
    let w = panel_width();
    let h = panel_height(parent_rows);
    let margin = MENU_EDGE_MARGIN;
    let mut left = parent_pos.x;
    let mut top = parent_pos.y;
    if left + w > win.width - margin {
        left = win.width - margin - w;
    }
    if left < margin {
        left = margin;
    }
    if top + h > win.height - margin {
        top = win.height - margin - h;
    }
    if top < margin {
        top = margin;
    }
    Bounds::new(point(left, top), size(w, h))
}

/// flyout 方向决策（纯函数）：**仅**依据输入判定，无任何测量回写 → 确定性、逐帧恒定
/// （中部 → Right/Down，与现状输出一致）。给 win 尺寸、父面板已夹持矩形、根项行顶、
/// 次级宽高、边缘留白。
pub fn flyout_direction(
    win: Size<Pixels>,
    panel: Bounds<Pixels>,
    row_top: Pixels,
    sub_w: Pixels,
    sub_h: Pixels,
    margin: Pixels,
) -> (FlyoutH, FlyoutV) {
    let h = if panel.right() + sub_w <= win.width - margin {
        FlyoutH::Right
    } else {
        FlyoutH::Left
    };
    let v = if panel.top() + row_top + sub_h <= win.height - margin {
        FlyoutV::Down
    } else {
        FlyoutV::Up
    };
    (h, v)
}

/// flyout 窗口矩形（由 [`flyout_direction`] 定方向 → 计算候选 → 自夹持进窗口边缘）。
/// 返回 `(方向, 左上角窗口坐标)`。
pub fn flyout_rect(
    win: Size<Pixels>,
    panel: Bounds<Pixels>,
    row_top: Pixels,
    sub_w: Pixels,
    sub_h: Pixels,
    margin: Pixels,
) -> (FlyoutH, FlyoutV, Point<Pixels>) {
    let (h, v) = flyout_direction(win, panel, row_top, sub_w, sub_h, margin);
    let row_bottom = row_top + ITEM_HEIGHT;
    let mut left = match h {
        FlyoutH::Right => panel.left() + panel_width(),
        FlyoutH::Left => panel.left() - sub_w,
    };
    let mut top = match v {
        FlyoutV::Down => panel.top() + row_top,
        FlyoutV::Up => panel.top() + row_bottom - sub_h,
    };
    // 自夹持（先压右/底、再压左/顶，防止窄窗内 min>max 反复翻转）。
    if left + sub_w > win.width - margin {
        left = win.width - margin - sub_w;
    }
    if left < margin {
        left = margin;
    }
    if top + sub_h > win.height - margin {
        top = win.height - margin - sub_h;
    }
    if top < margin {
        top = margin;
    }
    (h, v, point(left, top))
}


/// 单层下拉菜单视图（`Entity` 承载焦点与键盘状态）。
pub struct PopupMenu {
    focus_handle: FocusHandle,
    /// 开菜单前的焦点句柄，关闭时归还。
    previous_focus: Option<FocusHandle>,
    menu_items: Vec<PopupMenuItem>,
    selected_index: Option<usize>,
    /// 二级菜单「上一级」条目栈：进入子列表（[`PopupMenuItem::submenu`]）时压栈，返回
    /// （[`PopupMenuItem::back`]）时弹栈换回。只存一层深度通常够用，栈式可嵌套扩展。
    /// Dormancy：已被 [`PopupMenuItem::flyout`] 真悬浮路径取代，暂留作备用。
    sub_stack: Vec<Vec<PopupMenuItem>>,
    /// 真悬浮二级菜单：当前展开的**根项下标**（悬停/键盘选中带 flyout 的根项，或 Enter
    /// 激活时置位）。**持久化语义（2026-10 修订）**：一旦置位即保持展开——从根项移开进入
    /// 间隙/空白/次级面板都不会自动关闭（修复「慢速穿越间隙时次级先消失」竞态，见行为表）。
    /// 仅由四种情况改变：悬停/选中**另一个带 flyout 的根项**（切换下标）、**另一个无 flyout
    /// 的根项**（关闭）、Esc（先关次级）、整链 dismiss（全清）。
    open_flyout: Option<usize>,
    /// 次级菜单内当前悬停子项下标（面板内子项高亮；点击命中守卫 `on_mouse_down_out` 用）。
    /// 不再是「保持展开」的来源——展开与否由持久化的 `open_flyout` 决定。
    flyout_selected: Option<usize>,
    /// 指针是否在次级菜单**面板**内（覆盖子项间隙/边距）：点击守卫 `on_mouse_down_out`
    /// 据此放行「点次级面板预设不误关整链」。同样不再是「保持展开」的来源。
    flyout_panel_hover: bool,
    /// 父菜单在窗口中的**请求锚点**（左上角；宿主打开右键菜单时写入，见
    /// [`Self::set_parent_pos`]）。flyout 的窗口边缘空间适配据此 + 窗口尺寸用**常量估算**
    /// 做方向决策与自夹持（逐帧恒定，杜绝「snap 把 union 大盒回摆 → hover 抖动」振荡源）。
    /// `None` = 未设明确锚点（如 Dropdown 本地锚）→ flyout 走流式兄弟 fallback。
    parent_pos: Option<Point<Pixels>>,
}

impl PopupMenu {
    fn new(cx: &mut App) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            previous_focus: None,
            menu_items: Vec::new(),
            selected_index: None,
            sub_stack: Vec::new(),
            open_flyout: None,
            flyout_selected: None,
            flyout_panel_hover: false,
            parent_pos: None,
        }
    }

    /// 用构建器新建一个菜单实体（在实体内跑构建闭包，菜单项可经 `window/cx` 读动态状态）。
    pub fn build(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| f(Self::new(cx), window, cx))
    }

    /// 追加一个菜单项。
    pub fn item(mut self, item: PopupMenuItem) -> Self {
        self.menu_items.push(item);
        self
    }

    /// 追加一条分隔线（空菜单或已在尾部时不加，尾部分隔线渲染时忽略）。
    pub fn separator(mut self) -> Self {
        if self.menu_items.is_empty() {
            return self;
        }
        if let Some(last) = self.menu_items.last()
            && last.is_separator()
        {
            return self;
        }
        self.menu_items.push(PopupMenuItem::separator());
        self
    }

    /// 记下打开菜单前的焦点句柄（关闭时归还）。`Dropdown` 在元素内直接写字段；宿主在
    /// 窗口直接挂右键菜单时用它显式设置。
    pub fn set_previous_focus(&mut self, handle: Option<FocusHandle>) {
        self.previous_focus = handle;
    }

    /// 写入父菜单的窗口锚点（左上角请求位）。右键菜单宿主打开时调用（shell pdf/tab/media
    /// 均经 `deferred(anchored(Window)).position(pos)` 挂载）；启用 flyout 的窗口边缘适配。
    pub fn set_parent_pos(&mut self, pos: Point<Pixels>) {
        self.parent_pos = Some(pos);
    }

    /// 测试/诊断：返回 `(总条目数, 分割线数)`——总条目**含**分割线（项数 = 减分割线数），
    /// 用于断言菜单结构与分组（shell 文件树右键菜单按组用分隔线隔开）。`ElementItem`
    /// 的标签在渲染闭包内无法枚举，这里只数条目与分割线。
    #[allow(dead_code)] // shell 测试断言用
    pub fn layout_probe(&self) -> (usize, usize) {
        let separators = self
            .menu_items
            .iter()
            .filter(|i| i.is_separator())
            .count();
        (self.menu_items.len(), separators)
    }

    /// 测试/诊断：第 `index` 项是否被禁用（含分割线下标；越界/分割线 → `false`）。
    /// shell PDF 右键「唯一页时删除页面禁用」断言用。
    #[allow(dead_code)] // shell 测试断言用
    pub fn is_item_disabled(&self, index: usize) -> bool {
        match self.menu_items.get(index) {
            Some(PopupMenuItem::ElementItem { disabled, .. }) => *disabled,
            _ => false,
        }
    }

    /// 确认选中项：执行回调并关闭菜单（点击项 or Enter）。
    /// `keep_open` 项只执行回调、**不关闭**（如「打开最近…」保持文件菜单展开）。
    /// `swap_to`（二级交换）/ `back`（返回）项只在菜单内换条目、**不关闭**。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        let Some(item) = self.menu_items.get(ix) else {
            return;
        };
        let (disabled, handler, keep_open, swap_to, back, flyout) = match item {
            PopupMenuItem::ElementItem {
                disabled,
                handler,
                keep_open,
                swap_to,
                back,
                flyout,
                ..
            } => (
                *disabled,
                handler.clone(),
                *keep_open,
                swap_to.clone(),
                *back,
                flyout.is_some(),
            ),
            PopupMenuItem::Separator => return,
        };
        if disabled {
            return;
        }
        self.selected_index = Some(ix);
        // 二级交换：当前条目压栈，菜单内容原地换成子列表（不关闭、焦点保留）。
        // 顺带清 flyout 态：条目集被换走，旧 flyout 下标已失效。
        if let Some(sub) = swap_to {
            self.sub_stack
                .push(std::mem::replace(&mut self.menu_items, (*sub).clone()));
            self.selected_index = None;
            self.open_flyout = None;
            self.flyout_selected = None;
            self.flyout_panel_hover = false;
            cx.notify();
            return;
        }
        // 返回型：弹栈换回上一级条目（无上一级则忽略，保持现状）。
        if back {
            if let Some(prev) = self.sub_stack.pop() {
                self.menu_items = prev;
            }
            self.selected_index = None;
            self.open_flyout = None;
            self.flyout_selected = None;
            self.flyout_panel_hover = false;
            cx.notify();
            return;
        }
        // 带 flyout 的根项（子菜单开关、自身无回调）：Enter/点击 → **展开次级并保持菜单
        // 展开**（与 hover 一致）。此前 Enter 会让这种项「什么都不做就关闭」，键盘用户
        // 丢了背景色入口；点击根行也应开子菜单而非关闭（OS 菜单惯例）。
        if flyout {
            self.open_flyout = Some(ix);
            self.flyout_selected = None;
            self.flyout_panel_hover = false;
            cx.notify();
            return;
        }
        if let Some(handler) = handler {
            handler(&ClickEvent::default(), window, cx);
        }
        if !keep_open {
            self.dismiss(window, cx);
        }
    }

    /// 关闭菜单：清选中与 flyout 状态、发 `DismissEvent`（宿主据此收起）、归还焦点
    /// （若点击没带走焦点）。
    ///
    /// 焦点归还语义对齐 `recent_panel::RecentPanel::close` 的 `focus_left` 守卫：
    /// **仅当焦点仍留在菜单内**（`focus_still_inside` = 动作未把焦点移走，如 Esc /
    /// 点外 / 未移动焦点的普通项点击）才归还开菜单前的焦点；动作**已把焦点带走**（如
    /// flyout 子项 `open_file` → 聚焦新编辑器 / `open_dialog` → 原生对话框）则**不归还**
    /// ——否则 dismiss 会把动作刚拿到的焦点抢回无用的 `previous_focus` 句柄。
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_index = None;
        self.open_flyout = None;
        self.flyout_selected = None;
        self.flyout_panel_hover = false;
        cx.emit(DismissEvent);
        let focus_still_inside = self.focus_handle.contains_focused(window, cx);
        if focus_still_inside
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    fn select_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let cur = self.selected_index.unwrap_or(0);
        let prev = self
            .menu_items
            .iter()
            .enumerate()
            .rev()
            .find(|(i, item)| *i < cur && !item.is_separator() && !item_disabled(item));
        match prev {
            Some((ix, _)) => self.set_selected(ix, window, cx),
            None => {
                let last = self
                    .menu_items
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, item)| !item.is_separator() && !item_disabled(item));
                if let Some((ix, _)) = last {
                    self.set_selected(ix, window, cx);
                }
            }
        }
    }

    fn select_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let cur = self.selected_index;
        let next = match cur {
            Some(ix) => self
                .menu_items
                .iter()
                .enumerate()
                .skip(ix + 1)
                .find(|(_, item)| !item.is_separator() && !item_disabled(item)),
            None => self
                .menu_items
                .iter()
                .enumerate()
                .find(|(_, item)| !item.is_separator() && !item_disabled(item)),
        };
        if let Some((ix, _)) = next {
            self.set_selected(ix, window, cx);
        }
    }

    fn set_selected(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_index != Some(ix) {
            self.selected_index = Some(ix);
            // 键盘 ↑↓ 移到另一个根项：镜像悬停语义（移到带 flyout 项 → 切/开次级，
            // 移到普通项 → 关次级）——「移动到其他选项」规则对键盘路径同样成立。
            self.root_govern_flyout(ix);
            cx.notify();
        }
    }

    /// 确认选中**次级菜单**项：执行回调，按 `keep_open` 决定保留/关闭（语义同
    /// [`Self::activate`]）。次级面板只响应点击，不参与 swap/back（flyout 无返回项）。
    fn activate_flyout(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        window.prevent_default();
        let Some(items) = self
            .open_flyout
            .and_then(|root| match self.menu_items.get(root) {
                Some(PopupMenuItem::ElementItem { flyout: Some(items), .. }) => Some(items.clone()),
                _ => None,
            })
        else {
            return;
        };
        let Some(item) = items.get(ix) else {
            return;
        };
        let (disabled, handler, keep_open) = match item {
            PopupMenuItem::ElementItem {
                disabled,
                handler,
                keep_open,
                ..
            } => (*disabled, handler.clone(), *keep_open),
            PopupMenuItem::Separator => return,
        };
        if disabled {
            return;
        }
        self.flyout_selected = Some(ix);
        self.flyout_panel_hover = true;
        if let Some(handler) = handler {
            handler(&ClickEvent::default(), window, cx);
        }
        if !keep_open {
            self.dismiss(window, cx);
        }
    }

    /// 次级菜单是否**当前可见**：`open_flyout` 持久化后，只要「已置位 + 下标仍指向带 flyout
    /// 的根项」即为可见——不再要求根行/次级面板悬停源，因此指针停在根项与次级之间的
    /// 间隙/空白时次级保持展开（慢速穿越不丢）。与 `Render` 用同一谓词，保证 Esc 优先级
    /// 与可见态一致。
    fn flyout_visible(&self) -> bool {
        self.open_flyout.is_some_and(|ix| {
            self.menu_items.get(ix).is_some_and(|it| {
                matches!(it, PopupMenuItem::ElementItem { flyout: Some(_), .. })
            })
        })
    }

    /// 根项被悬停/键盘选中时的次级开/关治理（鼠标与键盘共用同一语义，见行为表）：
    /// 带 flyout 的根项 → 展开/切换为其次级（并清掉上一 flyout 的子项 armed 态，避免
    /// 新旧二次级高亮/点击守卫残留）；不带 flyout 的根项（= 普通选项）→ 关闭次级
    /// （「移动到其他选项」）。不改动 `selected_index`（高亮由调用方管理）。
    fn root_govern_flyout(&mut self, ix: usize) {
        let has_flyout = self.menu_items.get(ix).is_some_and(|it| {
            matches!(it, PopupMenuItem::ElementItem { flyout: Some(_), .. })
        });
        if has_flyout {
            self.flyout_selected = None;
            self.flyout_panel_hover = false;
            self.open_flyout = Some(ix);
        } else {
            // 关闭次级时**同步清 armed**（子项高亮/点击守卫）：否则关闭后残留
            // flyout_selected/panel_hover 会让下一次「点面板外」被守卫误拦、菜单关不掉
            // （键盘关次级/指针仍在原次级位置立即点击的边界路径）。与 Esc 分支同款清法。
            self.open_flyout = None;
            self.flyout_selected = None;
            self.flyout_panel_hover = false;
        }
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_ref() {
            "up" => self.select_up(window, cx),
            "down" => self.select_down(window, cx),
            "enter" => {
                if let Some(ix) = self.selected_index {
                    self.activate(ix, window, cx);
                } else if let Some(ix) = self.flyout_selected {
                    self.activate_flyout(ix, window, cx);
                }
            }
            "escape" => {
                cx.stop_propagation();
                // 次级菜单可见 → 首次 Esc 只关次级、焦点回到根项；父菜单保持展开。
                if self.flyout_visible() {
                    let root = self.open_flyout.filter(|&ix| {
                        self.menu_items.get(ix).is_some_and(|it| {
                            matches!(it, PopupMenuItem::ElementItem { flyout: Some(_), .. })
                        })
                    });
                    self.open_flyout = None;
                    self.flyout_selected = None;
                    self.flyout_panel_hover = false;
                    self.selected_index = root;
                    cx.notify();
                } else {
                    self.dismiss(window, cx);
                }
            }
            _ => {}
        }
    }

    fn on_mouse_down_out(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 单层菜单 + flyout：任何「根面板外」按下都关闭整链——但**次级面板内**的按下除外
        // （flyout 子项自身处理点击；根面板 `contains` 不含被平移出的次级面板，需按悬停态
        // 放行，否则点次级面板预设会被当成「面板外」误关整菜单）。
        // 放行以「次级当前**可见** 且 指针 armed 在次级上」为前提（双保险）：次级已关闭
        // 时即使 armed 残留也照常整链关闭，杜绝「关掉次级后点空白不关」的边界。
        if self.flyout_visible() && (self.flyout_selected.is_some() || self.flyout_panel_hover) {
            return;
        }
        let _ = e;
        self.dismiss(window, cx);
    }

    /// 公共菜单项行渲染（父菜单与 flyout 面板共用同一套：样式常量、选中/悬停高亮、
    /// hover/click 语义）。`is_flyout` 决定状态写入父选中索引还是 flyout 选中索引；
    /// `chevron` 非 None（=根项下标）时在行尾渲染「›」指示（flyout 根项）。
    #[allow(clippy::too_many_arguments)]
    fn menu_item_row(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ix: usize,
        item: &PopupMenuItem,
        selected: bool,
        is_flyout: bool,
        chevron: Option<usize>,
    ) -> AnyElement {
        let theme = Theme::global(cx);
        let colors = theme.colors;
        let fg: Hsla = colors.fg.into();
        let fg_disabled: Hsla = colors.fg_disabled.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        // Win11 中性菜单高亮：灰填充（非强调蓝 tint）；读**合并后** Theme（P1-2）。
        let selected_bg: Hsla = theme.chrome.menu_highlight.into();
        match item {
            PopupMenuItem::Separator => {
                let border: Hsla = colors.border.into();
                div()
                    .flex()
                    .h(px(1.))
                    .mx_1()
                    .my_1()
                    .bg(border.opacity(0.6))
                    .into_any_element()
            }
            PopupMenuItem::ElementItem { render, disabled, .. } => {
                let id_suffix = if is_flyout { "flyout-item" } else { "menu-item" };
                let sel_suffix = id_suffix;
                let this = div()
                    .id(ElementId::from(format!("{id_suffix}-{ix}")))
                    .debug_selector(move || format!("{sel_suffix}-{ix}"))
                    .flex()
                    .items_center()
                    .min_h(ITEM_HEIGHT)
                    .px(ITEM_PADDING)
                    .rounded_sm()
                    .gap_2()
                    .text_color(if *disabled { fg_disabled } else { fg })
                    .when(!disabled, |d| {
                        d.cursor_pointer()
                            .on_hover(cx.listener(move |this, hovered, _, cx| {
                                if *hovered {
                                    if is_flyout {
                                        // 次级菜单子项：悬停即高亮（面板内 armed）。持久化后
                                        // 展开与否不再依赖它——它只负责高亮与点击守卫。
                                        this.flyout_selected = Some(ix);
                                    } else {
                                        // 根项：悬停即高亮；次级开/关由「根项本身是否带 flyout」
                                        // 决定（切/开或关），与「悬停后是否离开」解耦 → 移开进
                                        // 间隙/空白不再误关次级（本轮核心修复）。
                                        this.selected_index = Some(ix);
                                        this.root_govern_flyout(ix);
                                    }
                                } else if is_flyout {
                                    // 次级子项移开：只清子项高亮，不动 open_flyout（面板保持）。
                                    if this.flyout_selected == Some(ix) {
                                        this.flyout_selected = None;
                                    }
                                } else if this.selected_index == Some(ix) {
                                    // 根项移开：只清高亮，**不关次级**——间隙/空白/途中驻留时
                                    // 次级保持（用户规则：没移动到其他选项就不会自动消失）。
                                    this.selected_index = None;
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if is_flyout {
                                    this.activate_flyout(ix, window, cx);
                                } else {
                                    this.activate(ix, window, cx);
                                }
                            }))
                    })
                    // P2-10：选中/悬停高亮淡入（键盘↑↓ 与鼠标同一曲线）。
                    .with_spring(
                        format!("{id_suffix}-bg-{ix}"),
                        crate::ui::motion::hover_spring(if selected { 1.0 } else { 0.0 }),
                        move |d, v| d.bg(selected_bg.opacity(v)),
                    );
                this.child((render)(window, cx))
                    .when_some(chevron, |d, root_ix| {
                        d.child(
                            div()
                                .id(format!("menu-item-chevron-{root_ix}"))
                                .ml_auto()
                                .text_color(fg_muted)
                                .child("›")
                                .debug_selector(move || format!("menu-flyout-chevron-{root_ix}")),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

fn item_disabled(item: &PopupMenuItem) -> bool {
    match item {
        PopupMenuItem::ElementItem { disabled, .. } => *disabled,
        PopupMenuItem::Separator => true,
    }
}

impl EventEmitter<DismissEvent> for PopupMenu {}
impl Focusable for PopupMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PopupMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        let surface: Hsla = colors.surface.into();
        let fg: Hsla = colors.fg.into();
        // 浮层投影底色 = 主题 card_shadow（默认不透明黑，与旧散值 `hsla(0,0,0,α)` 逐通道同值）。
        let shadow_base: Hsla = colors.card_shadow.into();
        let items_count = self.menu_items.len();
        let selected = self.selected_index;

        // 次级菜单是否展开：open_flyout 持久化（一旦展开保持——间隙/空白驻留不关；悬停/
        // 选中普通根项、Esc 或整链 dismiss 才变化）。
        let flyout_ix = self
            .open_flyout
            .filter(|_| self.flyout_visible());

        let items = self
            .menu_items
            .iter()
            .enumerate()
            // 忽略尾部分隔线（与 gpui-component 对齐）
            .filter(|(ix, item)| !(*ix + 1 == items_count && item.is_separator()))
            .map(|(ix, item)| {
                // flyout 根项行右侧带「›」指示。
                let chevron = match item {
                    PopupMenuItem::ElementItem { flyout: Some(_), .. } => Some(ix),
                    _ => None,
                };
                self.menu_item_row(window, cx, ix, item, selected == Some(ix), false, chevron)
            })
            .collect::<Vec<_>>();

        // 父菜单面板（含边框/阴影/圆角 chrome）：根元素为透明 flex-row，把父/次级面板
        // 作为**流式兄弟**排布——次级面板左缘天然落在父面板右缘（规格 4），顶部经
        // `mt(row_top)` 下移到 flyout 根项行顶部（规格 3），不作绝对定位/猜宽。
        // 点击外关闭挂在**父面板**（而非 union 根盒）：flyout 是流式兄弟会把根盒撑高，
        // 挂在根盒会把「面板下方空白」误当内部 → 点空白不关闭。
        let items_column = div()
            .id("items")
            .debug_selector(|| "menu-panel".into())
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            // 菜单弹出层存活期间的左键按下拦截（bubble 阶段 + 命中面板才拦）：gpui 鼠标
            // 事件按监听器注册顺序派发（bubble 逆序），弹层（`deferred(anchored())`）监听器
            // 晚于主层 viewer 注册 → 这里先于下层 PdfViewer 的 `on_mouse_down` stop →
            // 弹层内左键按下到不了 viewer。若不拦，viewer 命中文本即置 `selecting=true`，
            // 而 mouseup 被菜单项 on_click 的 stop_propagation 吃掉 → viewer 收不到 mouseup
            // → selecting 滞留 → 之后移动鼠标都被当成拖选（用户反馈根因）。注意必须用
            // bubble 而非 capture：capture stop 会跳过整轮 bubble，菜单项自身的
            // pending_mouse_down（click 依赖）也在 bubble 记录 → 点不了菜单。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .p(PANEL_PADDING)
            .gap_y(PANEL_PADDING)
            .min_w(PANEL_MIN_W)
            .bg(surface.opacity(if is_dark {
                crate::ui::theme::FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                crate::ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT
            }))
            .border_1()
            .border_color(colors.border)
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(8.), shadow_base.opacity(0.20))
                    .blur_radius(px(14.)),
                gpui::BoxShadow::new(px(0.), px(2.), shadow_base.opacity(0.14))
                    .blur_radius(px(4.)),
            ])
            .rounded_lg()
            .text_color(fg)
            .children(items);

        // 次级菜单面板：与父面板同款样式常量。宿主提供窗口锚点（`parent_pos`）时走
        // **绝对锚定 + 窗口边缘自夹持**（方向决策纯函数 + 翻左/翻上，逐帧恒定、不回流），
        // 消除「流式兄弟把父根盒撑成 union → shell anchor snap 逐帧回摆 → hover 抖动」的
        // 振荡源；未设锚点（如 Dropdown 本地锚）退回流式兄弟 fallback（现状行为）。
        let flyout_body = flyout_ix.and_then(|root_ix| {
            let sub = match self.menu_items.get(root_ix) {
                Some(PopupMenuItem::ElementItem { flyout: Some(sub), .. }) => sub.clone(),
                _ => return None,
            };
            let fsel = self.flyout_selected;
            let frows = sub
                .iter()
                .enumerate()
                .filter(|(i, item)| !(*i + 1 == sub.len() && item.is_separator()))
                .map(|(i, item)| self.menu_item_row(window, cx, i, item, fsel == Some(i), true, None))
                .collect::<Vec<_>>();
            let row_top = MENU_BORDER
                + PANEL_PADDING
                + self.menu_items[..root_ix]
                    .iter()
                    .fold(Pixels::ZERO, |acc, it| acc + row_slot(it));
            let fpanel = div()
                .id("flyout-panel")
                .debug_selector(|| "menu-flyout".into())
                .flex()
                .flex_col()
                .p(PANEL_PADDING)
                .gap_y(PANEL_PADDING)
                .min_w(PANEL_MIN_W)
                .children(frows)
                // 面板级 hover：覆盖子项间隙/边距，作为保持展开的第三源（防「根行→次级」误关）。
                .on_hover(cx.listener(|this, hovered, _, cx| {
                    this.flyout_panel_hover = *hovered;
                    cx.notify();
                }))
                // 次级面板同样拦截左键按下（bubble）：flyout 次级是独立 deferred 层，父面板
                // 的拦截不覆盖它——否则点次级内的 mousedown 仍会穿透到下层 viewer。
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .bg(surface.opacity(if is_dark {
                    crate::ui::theme::FLOAT_ACRYLIC_ALPHA_DARK
                } else {
                    crate::ui::theme::FLOAT_ACRYLIC_ALPHA_LIGHT
                }))
                .border_1()
                .border_color(colors.border)
                .shadow(vec![
                    gpui::BoxShadow::new(px(0.), px(8.), shadow_base.opacity(0.20))
                        .blur_radius(px(14.)),
                    gpui::BoxShadow::new(px(0.), px(2.), shadow_base.opacity(0.14))
                        .blur_radius(px(4.)),
                ])
                .rounded_lg()
                .text_color(fg);

            match self.parent_pos {
                // 窗口锚定 + 边缘适配：父面板自夹持（镜像 shell snap）→ 方向 → 次级矩形
                // → Local 绝对定位。次级以根布局原点（= 父面板窗口位置）为基准偏移，绝对
                // 定位不参与父根盒布局 → 无 union 增长、无 snap 反馈振荡。
                Some(pos) => {
                    let win = window.viewport_size();
                    let panel_rect = parent_panel_rect(pos, win, &self.menu_items);
                    let (_h, _v, fly_top_left) = flyout_rect(
                        win,
                        panel_rect,
                        row_top,
                        panel_width(),
                        panel_height(&sub),
                        MENU_EDGE_MARGIN,
                    );
                    let rel = point(
                        fly_top_left.x - panel_rect.left(),
                        fly_top_left.y - panel_rect.top(),
                    );
                    Some(
                        deferred(
                            anchored()
                                .position_mode(gpui::AnchoredPositionMode::Local)
                                .anchor(Anchor::TopLeft)
                                .position(rel)
                                // 显式覆写默认 `SwitchAnchor`：Local 模式下 gpuISwitchAnchor 会把
                                // 相对 `rel` 当窗口坐标做翻边判定，一旦估算偏差把次级任一缘推出
                                // 窗口就会把次级弹到错误角落。`flyout_rect` 已自夹在
                                // [margin, win-margin]（镜像 snap 语义），此处再以同 margin
                                // re-snap 是精确 no-op，只是把 fit_mode 钉成安全值。
                                .snap_to_window_with_margin(Edges::all(MENU_EDGE_MARGIN))
                                .child(fpanel),
                        )
                        .with_priority(100)
                        .into_any_element(),
                    )
                }
                // fallback：无锚点宿主 → 流式兄弟（Dropdown 等本地锚，现无 flyout 场景）。
                None => Some(fpanel.mt(row_top).into_any_element()),
            }
        });

        div()
            .id("popup-menu")
            .debug_selector(|| "popup-menu".into())
            // flex-row：父/次级面板流式并排，次级左缘 = 父面板右缘（规格 4）。
            // `items_start`：**禁用默认 cross-axis stretch**——否则父面板会被次级
            // 的 `mt(row_top)` 拉伸到极大高度（真实 PDF 排列 182→341px），既有观感
            // 异常又让「面板下方空白」失去关闭边界。对齐固定为 flex-start 后两个面板
            // 各按其自然高布局，次级仅靠 `mt(row_top)` 下移到根项行顶（规格 3）。
            .flex()
            .items_start()
            .role(accesskit::Role::Menu)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(items_column)
            .when_some(flyout_body, |this, fb| this.child(fb))
    }
}

/// 宿主右键菜单槽位「三件套」（pos/entity/sub）收敛：shell pdf / 媒体锁定 / 选项卡栏 /
/// 侧栏文件树各处原本各持三个 `Option` 字段（位置 / 菜单实体 / 关闭订阅），收成本
/// struct 一块管理，`clear()` 一次关净、`is_open()` 判开。
/// 实体类型各站点统一为 `Entity<PopupMenu>`，故不用泛型。
#[derive(Default)]
pub struct MenuSlot {
    /// 菜单弹出位置（窗口坐标）；与 `entity` 同生同灭。
    pub pos: Option<Point<Pixels>>,
    /// 已打开的菜单实体（`None` = 未打开）。
    pub entity: Option<Entity<PopupMenu>>,
    /// 菜单关闭订阅（选择/Esc/点击外 → 宿主清槽位）。
    pub sub: Option<Subscription>,
}

impl MenuSlot {
    /// 关闭：置空位置/实体/订阅（订阅随 drop 取消，宿主不再收到后续关闭回调）。
    /// 与「关闭时逐字段置 `None`」行为一致，一处收口。
    pub fn clear(&mut self) {
        self.pos = None;
        self.entity = None;
        self.sub = None;
    }

    /// 是否已打开菜单（以实体在场为准；`pos` 与 `entity` 同生同灭）。
    pub fn is_open(&self) -> bool {
        self.entity.is_some()
    }
}

/// 右键菜单 Overlay 助手：封装各宿主（shell pdf / 媒体锁定 / 选项卡栏 / 侧栏文件树）
/// 复用的 `deferred(anchored(Window)).position(pos).snap_to_window_with_margin(margin)
/// .with_priority(100)` 骨架——`position()` 才是 gpui 语义里的「窗口坐标」入参
/// （`offset()` 会被当相对锚定元素静态布局位置的偏移）；`margin` 传
/// [`MENU_EDGE_MARGIN`]（与 PopupMenu 自夹持同一常量，父面板顶/左估算逐像素一致）。
/// 调用方需要附加 `id`/`debug_selector` 外壳时自行包装（如选项卡栏/文件树菜单）。
pub fn popup_overlay(
    pos: Point<Pixels>,
    menu: Entity<PopupMenu>,
    margin: Pixels,
) -> impl IntoElement {
    deferred(
        anchored()
            .position_mode(gpui::AnchoredPositionMode::Window)
            .anchor(Anchor::TopLeft)
            .position(pos)
            .snap_to_window_with_margin(Edges::all(margin))
            .child(menu),
    )
    .with_priority(100)
}

/// 右键上下文菜单助手：一处收敛「建 `PopupMenu` + `set_previous_focus` + `focus` +
/// `subscribe(DismissEvent)` + 宿主写好状态」近 30 行×2 同构样板（shell tab 栏右键 /
/// 媒体锁定右键）。
///
/// 封装闭环：
/// 1. `PopupMenu::build(window, cx, items)`——`items` 在菜单实体内运行（可 push 条目 /
///    读动态状态）；
/// 2. **焦点管理**：记下开菜单前的焦点（`PopupMenu` 关闭时归还）+ 聚焦菜单；
/// 3. **dismiss 订阅**：`DismissEvent`（选择 / Esc / 点击外）→ 调 `on_dismiss`
///    （宿主在里面对照 emitter 做**身份校验**后清自己的槽位 + `cx.notify()`），
///    杜绝旧菜单的关闭误清新菜单。
///
/// 返回 `(menu, sub)` 句柄：宿主把 `menu` 与 `position` 存入自己的槽位（渲染时
/// `deferred(anchored(Window)).position(pos)` 定位），`sub` 持住订阅；槽位字段存闭仍是
/// 宿主职责（`Context<T>` 拿不到 `&mut T`，见 §2.6 派生状态副作用纪律）。
pub fn spawn_context_menu<T>(
    window: &mut Window,
    cx: &mut Context<T>,
    items: impl FnOnce(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    on_dismiss: impl Fn(&mut T, &Entity<PopupMenu>, &mut Context<T>) + 'static,
) -> (Entity<PopupMenu>, Subscription)
where
    T: 'static,
{
    let menu = PopupMenu::build(window, cx, items);

    // 记下开菜单前的焦点（关闭归还）+ 聚焦菜单。
    let previous_focus = window.focused(cx);
    let focus_handle = menu.read(cx).focus_handle(cx);
    menu.update(cx, |m, _| m.set_previous_focus(previous_focus));
    window.focus(&focus_handle, cx);

    // 关闭（选择/Esc/点击外）→ 宿主按 emitter 身份校验清理自己的菜单槽位。
    let sub = cx.subscribe(&menu, move |this, emitter, _: &DismissEvent, cx| {
        on_dismiss(this, &emitter, cx);
    });

    (menu, sub)
}

/// 菜单加载器：给定空菜单 + 宿主窗口，返回装配好的菜单。
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// 触发宿主：把按钮 + 按下即开/关的菜单包成一个元素。
pub struct Dropdown {
    id: ElementId,
    trigger: crate::ui::button::Button,
    menu_builder: MenuBuilder,
}

/// 元素级跨帧状态（按宿主 id 隔离）。
#[derive(Default)]
struct DropdownState {
    open: bool,
    menu: Option<Entity<PopupMenu>>,
}

impl Dropdown {
    fn new(id: impl Into<ElementId>, trigger: crate::ui::button::Button) -> Self {
        Self {
            id: id.into(),
            trigger,
            menu_builder: Rc::new(|menu, _, _| menu),
        }
    }

    fn with_menu(
        mut self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.menu_builder = Rc::new(f);
        self
    }
}

impl RenderOnce for Dropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state =
            window.use_keyed_state((self.id.clone(), "dropdown"), cx, |_, _| DropdownState::default());
        let view_id = window.current_view();
        let open = state.read(cx).open;

        // 打开时确保已有菜单实体：惰性构建 + 聚焦 + 订阅 DismissEvent（关闭时收敛状态）。
        if open
            && let Some(menu) = {
                match state.read(cx).menu.clone() {
                    Some(menu) => Some(menu),
                    None => {
                        let builder = self.menu_builder.clone();
                        let menu = PopupMenu::build(window, cx, move |pm, window, cx| {
                            (builder)(pm, window, cx)
                        });
                        let previous_focus = window.focused(cx);
                        menu.update(cx, |m, _| m.previous_focus = previous_focus);
                        menu.read(cx).focus_handle(cx).focus(window, cx);

                        let sub_state = state.clone();
                        // 菜单被关闭（选择/Esc/点击外）→ 宿主同时收起。
                        window
                            .subscribe(&menu, cx, move |_, _: &DismissEvent, _window, cx| {
                                sub_state.update(cx, |s, _| {
                                    s.open = false;
                                    s.menu = None;
                                });
                                cx.notify(view_id);
                            })
                            .detach();
                        state.update(cx, |s, _| s.menu = Some(menu.clone()));
                        Some(menu)
                    }
                }
            }
        {
            let menu = menu;
            let _ = &menu; // 借住实体生命周期（已存入 state）
        }

        let menu_view = state.read(cx).menu.clone();

        let trigger = self.trigger;
        div()
            .id(self.id.clone())
            .relative()
            // 触发开关放在「包着按钮的包装层」上（与菜单是兄弟，菜单内按下不会冒泡到这，
            // 不误切换）；菜单自己处理选择/Esc/点击外关闭。
            .child(
                div()
                            .on_mouse_down(
                                MouseButton::Left,
                                move |_, _window, cx| {
                                    let was_open = state.read(cx).open;
                                    state.update(cx, |s, _| {
                                        s.open = !was_open;
                                        if was_open {
                                            s.menu = None;
                                        }
                                    });
                                    cx.notify(view_id);
                                },
                            )
                    .child(trigger),
            )
            .when_some(menu_view, |this, menu| {
                this.child(
                    deferred(
                        anchored()
                            .position_mode(gpui::AnchoredPositionMode::Local)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(Edges::all(px(8.)))
                            .child(menu),
                    )
                    .with_priority(100),
                )
            })
    }
}

impl IntoElement for Dropdown {
    type Element = gpui::ViewElement<Self>;

    #[track_caller]
    fn into_element(self) -> Self::Element {
        gpui::ViewElement::new(self)
    }
}

/// 给按钮挂下拉菜单：`button.dropdown_menu(|menu, _, _| …)` → [`Dropdown`]。
pub trait DropdownMenu: Sized {
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Dropdown;
}

impl DropdownMenu for crate::ui::button::Button {
    fn dropdown_menu(
        self,
        f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Dropdown {
        Dropdown::new(self.element_id(), self).with_menu(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::button::{Button, ButtonVariants as _};
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext, point, px};
    use std::{cell::Cell, rc::Rc};

    struct Harness {
        clicks: Rc<Cell<usize>>,
    }

    /// 直接持有并渲染 `PopupMenu` 实体的宿主（`Dropdown` 也是把它挂进元素树，
    /// 这里省去触发按钮，聚焦测试的滚动容器/滚动条结构）。
    struct MenuHost {
        menu: Entity<PopupMenu>,
    }

    impl Render for MenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("menu-host")
                .debug_selector(|| "menu-host".into())
                .w(px(400.))
                .h(px(400.))
                // flex-row + items_start：菜单根元素是 flex 容器，需给它内容宽（而非块级
                // 拉伸到宿主全宽），与 shell 侧 `anchored()` 的内容宽语义一致。
                .flex()
                .items_start()
                .child(self.menu.clone())
        }
    }

    /// 模拟 shell 的 `deferred(anchored(Window)).position(pos)` 宿主：菜单在窗口坐标
    /// (50,50) 处弹出。用于验证 anchored 内容宽语义下 flyout 几何与 flex 宿主一致。
    struct AnchoredMenuHost {
        menu: Entity<PopupMenu>,
    }

    impl Render for AnchoredMenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("anchored-host")
                .w(px(600.))
                .h(px(500.))
                .child(popup_overlay(
                    point(px(50.), px(50.)),
                    self.menu.clone(),
                    MENU_EDGE_MARGIN,
                ))
        }
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            div()
                .id("menu-host")
                .w(px(400.))
                .h(px(300.))
                .child(
                    Button::new("menu-trigger")
                        .label("菜单")
                        .ghost()
                        .w(px(80.))
                        .h(px(30.))
                        .dropdown_menu(move |menu, _, _| {
                            let clicks = clicks.clone();
                            menu.item(
                                PopupMenuItem::element(|_, _| div().child("第一条"))
                                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                            )
                            .separator()
                            .item(PopupMenuItem::element(|_, _| div().child("第二条")))
                        }),
                )
        }
    }

    fn harness(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Harness { clicks }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        (cx, clicks)
    }

    /// 触发按钮中心（宿主左上，菜单经绝对定位挂在按钮正下方，不重叠）。
    const TRIGGER: (f32, f32) = (40., 15.);
    /// 首项可见区内的点（菜单顶端约在按钮底 y≈30，首项 y≈34..60）。
    const ITEM: (f32, f32) = (40., 48.);
    /// 菜单与触发区之外的空白点。
    const OUTSIDE: (f32, f32) = (300., 200.);

    #[gpui::test]
    fn click_trigger_opens_and_item_activates(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        // 关闭态点触发按钮 → 打开菜单。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        // 点首项 → 触发回调且只触发一次。
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "点菜单首项应触发一次回调");

        // 再点触发按钮（点项后已自动关闭）→ 重新打开；再点首项 → 又触发一次。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 2, "重新打开后点首项仍只触发一次");
    }

    #[gpui::test]
    fn keyboard_select_and_enter_activates(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        // 菜单聚焦后：↓ 选中首项 → Enter 确认执行并关闭。
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "↓+Enter 应激活首项回调");
    }

    #[gpui::test]
    fn escape_and_click_outside_dismiss(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks) = harness(cx);

        // 打开 → Esc 关闭。关闭后点首项位置回到触发按钮（打开），不再触发回调。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 0, "Esc 关闭后点击不应触发回调");

        // 打开 → 点空白处关闭。
        cx.simulate_click(point(px(TRIGGER.0), px(TRIGGER.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(OUTSIDE.0), px(OUTSIDE.1)), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(point(px(ITEM.0), px(ITEM.1)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 0, "点空白关闭后点击不应触发回调");
    }

    /// `keep_open` 项点击后菜单**不关闭**（DismissEvent 不发、焦点保留）；普通项
    /// 点击仍关闭（既有语义）。
    #[gpui::test]
    fn keep_open_item_does_not_dismiss(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let dismisses = dismisses.clone();
            move |window, cx| {
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    let keep = PopupMenuItem::element(|_, _| div().child("More"))
                        .keep_open()
                        .on_click(|_, _, _| {});
                    let close = PopupMenuItem::element(|_, _| div().child("关闭项"))
                        .on_click(|_, _, _| {});
                    menu.item(keep).item(close)
                });
                // 订阅 DismissEvent 计数：keep_open 项不得触发、普通项应触发。
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        // 首项（"More"，keep_open）：点击后菜单不关闭（dismiss 计数仍 0）。
        cx.simulate_click(point(px(40.), px(17.)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            dismisses.get(),
            0,
            "keep_open 项点击不应触发菜单关闭（dismiss）"
        );
        // 第二项（普通项）：点击应关闭菜单（dismiss 计数 1）。
        cx.simulate_click(point(px(40.), px(47.)), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 1, "普通项点击应关闭菜单（dismiss=1）");
    }

    // ---------------------------------------------------------------------
    // `spawn_context_menu`：右键菜单生命周期闭环（焦点管理 + dismiss 身份校验清槽位）。
    // ---------------------------------------------------------------------

    /// 右键菜单宿主：持 [`MenuSlot`]（mimic shell 的「entity / pos / sub」三槽位），作
    /// `spawn_context_menu` 生命周期载体。
    struct ContextMenuHost {
        menu: MenuSlot,
    }

    impl Render for ContextMenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let content = match &self.menu.entity {
                Some(menu) => div()
                    .id("ctx-menu-host")
                    .size_full()
                    .child(menu.clone())
                    .into_any_element(),
                None => div().id("ctx-menu-host").size_full().into_any_element(),
            };
            content
        }
    }

    /// 打开：`spawn_context_menu` 建菜单 + 记/归还焦点 + 订阅；Esc 关闭 → dismiss
    /// 订阅按 emitter 身份校验命中当前槽位 → 清 entity/pos 槽位（宿主重绘）。
    #[gpui::test]
    fn context_menu_spawn_focuses_and_dismiss_cleans_slot(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut host = ContextMenuHost {
                menu: MenuSlot::default(),
            };
            let (menu, sub) = spawn_context_menu(
                window,
                cx,
                |menu, _, _| {
                    menu.item(
                        PopupMenuItem::element(|_, _| div().child("右键菜单项"))
                            .on_click(|_, _, _| {}),
                    )
                },
                // 宿主侧：emitter 身份校验命中当前槽位才清理（旧菜单关闭不会误清新菜单）。
                |this: &mut ContextMenuHost,
                 emitter: &Entity<PopupMenu>,
                 cx: &mut Context<ContextMenuHost>| {
                    if this.menu.entity.as_ref() == Some(&emitter) {
                        this.menu.clear();
                    }
                    cx.notify();
                },
            );
            host.menu.pos = Some(point(px(10.), px(10.)));
            host.menu.entity = Some(menu);
            host.menu.sub = Some(sub);
            host
        });
        // 打开态：菜单已聚焦且槽位写满。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let (open_entity, open_pos) = cx.update(|_, cx| {
            (
                view.read(cx).menu.entity.is_some(),
                view.read(cx).menu.pos.is_some(),
            )
        });
        assert!(open_entity, "打开后实体槽位应有菜单");
        assert!(open_pos, "打开后位置槽位应有坐标");
        // Esc 关闭菜单 → DismissEvent → 订阅按身份校验清理槽位。
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let (gone_entity, gone_pos) = cx.update(|_, cx| {
            (
                view.read(cx).menu.entity.is_none(),
                view.read(cx).menu.pos.is_none(),
            )
        });
        assert!(gone_entity, "dismiss 后实体槽位应被清理");
        assert!(gone_pos, "dismiss 后位置槽位应被清理");
    }

    /// 二级菜单「原地交换」：`submenu` 项点击 → 菜单内容换成子列表（不触发 dismiss、
    /// 菜单保持展开）；子列表普通 `keep_open` 项执行回调不关闭；「返回」（`back`）点击 →
    /// 换回上一级。复用 `MenuHost` 渲染 + `debug_bounds` 定位断言内容切换。
    #[gpui::test]
    fn submenu_swaps_items_and_back_restores(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let clicks = Rc::new(Cell::new(0usize));
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub_click = clicks.clone();
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    // 子列表：keep_open 子项 + 返回项。
                    let sub = vec![
                        PopupMenuItem::element(move |_, _| {
                            div().debug_selector(|| "submenu-a".into()).child("子项A")
                        })
                        .keep_open()
                        .on_click(move |_, _, _| sub_click.set(sub_click.get() + 1)),
                        PopupMenuItem::separator(),
                        PopupMenuItem::element(|_, _| {
                            div().debug_selector(|| "submenu-back".into()).child("返回")
                        })
                        .back(),
                    ];
                    // 根：一级 submenu 项（换入子列表）+ 普通关闭项。
                    let d = dismisses.clone();
                    menu.item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "submenu-root".into())
                                .child("背景色")
                        })
                        .submenu(sub),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div().debug_selector(|| "submenu-close".into()).child("关闭项")
                        })
                        .on_click(move |_, _, _| d.set(d.get() + 1)),
                    )
                });
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // 一级：点「背景色」→ 菜单内容原地换成子列表（根项消失、子项出现），不 dismiss。
        let root = cx.debug_bounds("submenu-root").expect("根项应在场").center();
        cx.simulate_click(root, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 0, "进入子列表不应关闭菜单");
        assert!(cx.debug_bounds("submenu-root").is_none(), "根项应被替换");
        assert!(cx.debug_bounds("submenu-a").is_some(), "子项A应出现");

        // 子列表 keep_open 项：执行回调、不关闭菜单。
        let sub = cx.debug_bounds("submenu-a").expect("子项A应在场").center();
        cx.simulate_click(sub, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "keep_open 子项应触发一次回调");
        assert_eq!(dismisses.get(), 0, "keep_open 子项不应关闭菜单");

        // 返回项：换回上一级（子项消失、根项恢复），不 dismiss。
        let back = cx.debug_bounds("submenu-back").expect("返回应在场").center();
        cx.simulate_click(back, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 0, "返回不应关闭菜单");
        assert!(cx.debug_bounds("submenu-root").is_some(), "返回后根项应恢复");
        assert!(cx.debug_bounds("submenu-a").is_none(), "返回后子项应消失");

        // 再进一次再返回：栈可重复进出（不随上次残留）。
        let root2 = cx.debug_bounds("submenu-root").expect("根项应在场").center();
        cx.simulate_click(root2, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("submenu-a").is_some(), "再次进入子列表应生效");
        let back2 = cx.debug_bounds("submenu-back").expect("返回应在场").center();
        cx.simulate_click(back2, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("submenu-root").is_some(), "再次返回应恢复根项");
    }

    // ---------------------------------------------------------------------
    // 真悬浮二级菜单（flyout）：悬停根项 → 次级菜单在父菜单右侧展开（父项保持可见、
    // 不原地替换）；根行/次级面板双源防抖；几何（次级顶部 ≈ 根项行顶部、左缘 ≈ 父菜单
    // 右缘，容差内）；点击次级 keep_open 执行回调不关闭；Esc 先关次级再关父；点外整链关。
    // ---------------------------------------------------------------------

    /// flyout 几何容差（顶部/左缘允许 ≤5px 偏差）。
    const FLYOUT_GEOM_TOL: f32 = 5.;

    /// 建「普通关闭项 + 背景色(flyout×2) + 普通项」菜单宿主；返回 click/dismiss 计数。
    fn flyout_harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0usize));
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub_click = clicks.clone();
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    let sub = vec![
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .debug_selector(|| "flyout-a".into())
                                .child("默认")
                                .into_any_element()
                        })
                        .keep_open()
                        .on_click(move |_, _, _| sub_click.set(sub_click.get() + 1)),
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "flyout-b".into())
                                .child("夜间")
                                .into_any_element()
                        })
                        .keep_open()
                        .on_click(|_, _, _| {}),
                    ];
                    menu.item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-close".into())
                                .child("关闭项")
                                .into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-bg".into())
                                .child("背景色")
                                .into_any_element()
                        })
                        .flyout(sub),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-insert".into())
                                .child("插入项")
                                .into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                });
                // 聚焦菜单：Esc 两段关闭测试需要键盘焦点。
                menu.read(cx).focus_handle(cx).focus(window, cx);
                // 写入父菜单锚点（MenuHost 流式排版菜单位于窗口 (0,0)）→ 激活 flyout 的
                // 窗口边缘适配路径（绝对锚定 + 自夹持），与 shell 侧语义一致。
                menu.update(cx, |m, _| m.set_parent_pos(point(px(0.), px(0.))));
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, clicks, dismisses)
    }

    /// 把指针移到某 debug_selector 项中心（触发 hover）。
    fn hover_center(cx: &mut VisualTestContext, id: &'static str) {
        let p = cx.debug_bounds(id).unwrap_or_else(|| panic!("hover 目标 {id} 应在场")).center();
        cx.simulate_mouse_move(p, None, Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn flyout_hover_opens_parent_items_stay_visible_and_closes_on_normal_item(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = flyout_harness(cx);

        // 初始：父三项可见、无 flyout、根行带「›」指示。
        assert!(cx.debug_bounds("root-close").is_some(), "初始关闭项应在场");
        assert!(cx.debug_bounds("root-bg").is_some(), "初始背景色应在场");
        assert!(cx.debug_bounds("root-insert").is_some(), "初始插入项应在场");
        assert!(cx.debug_bounds("flyout-a").is_none(), "未悬停时次级不应在场");
        assert!(
            cx.debug_bounds("menu-flyout-chevron-1").is_some(),
            "背景色根行应带 › 箭头指示"
        );

        // 悬停「背景色」→ 次级菜单出现；**父项全部保持可见**（不原地替换）。
        hover_center(cx, "root-bg");
        let fly = cx.debug_bounds("menu-flyout");
        let root_bg = cx.debug_bounds("root-bg");
        let root_close = cx.debug_bounds("root-close");
        let root_insert = cx.debug_bounds("root-insert");
        assert!(fly.is_some(), "悬停背景色后次级菜单应出现");
        assert!(root_bg.is_some() && root_close.is_some() && root_insert.is_some(),
            "父菜单其它项应保持可见（不原地替换）");
        assert!(cx.debug_bounds("flyout-a").is_some(), "次级应含默认项");
        assert!(cx.debug_bounds("flyout-b").is_some(), "次级应含夜间项");

        // 悬停到次级子项 → 保持展开（根行已离开、子项接棒的第二源）。
        hover_center(cx, "flyout-a");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "进入次级菜单应保持展开");
        assert!(cx.debug_bounds("flyout-b").is_some(), "次级其余项应仍在场");

        // 悬停到普通「插入项」→ 次级关闭、父项仍在场。
        hover_center(cx, "root-insert");
        assert!(cx.debug_bounds("menu-flyout").is_none(), "悬停普通项应关闭次级");
        assert!(cx.debug_bounds("root-bg").is_some(), "父项保持在场");
    }

    /// 慢速穿越回归（用户反馈核心）：打开次级后把指针移到「根项行之间的空隙 / 父面板空白」
    /// 驻留 ≥10 帧，次级**必须保持展开**——旧实现根项 hover 离开即清 selected_index、
    /// flyout_visible 双源全灭 → 次级在鼠标到达前消失；本轮 open_flyout 持久化后不丢。
    #[gpui::test]
    fn flyout_persists_while_pointer_dwells_in_gap(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = flyout_harness(cx);
        hover_center(cx, "root-bg");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停背景色应展开次级");

        // 取「背景色行」与「插入项行」之间的行间空隙中点（父面板内、不落在任何根项上，
        // x 在次级面板左侧）——真实慢速穿越最常驻留的位置。
        let row1 = cx.debug_bounds("menu-item-1").expect("背景色行应在场");
        let row2 = cx.debug_bounds("menu-item-2").expect("插入项行应在场");
        assert!(row1.bottom() < row2.top(), "行间应有空隙（rows {row1:?} / {row2:?}）");
        let gap = gpui::point(
            row1.left() + px(20.),
            row1.bottom() + (row2.top() - row1.bottom()) * 0.5,
        );
        cx.simulate_mouse_move(gap, None, Modifiers::default());
        cx.run_until_parked();

        // 驻留 ≥10 帧（模拟慢速移动在途中停留——旧实现此刻次级已消失）。
        for _ in 0..10 {
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert!(
            cx.debug_bounds("menu-flyout").is_some(),
            "间隙/空白驻留 ≥10 帧次级应保持展开（慢速穿越不丢）"
        );
        assert!(cx.debug_bounds("flyout-a").is_some(), "次级子项应仍在场");
        assert!(cx.debug_bounds("root-bg").is_some(), "父项仍在场");

        // 从间隙移到无 flyout 的普通根项 → 关闭次级（行为表：「移动到其他选项」）。
        hover_center(cx, "root-insert");
        assert!(cx.debug_bounds("menu-flyout").is_none(), "移到普通根项应关闭次级");
    }

    /// 悬停到**另一个带 flyout 的根项** → 次级**切换**为该根项的子列表（旧 A 关、新 B 开）；
    /// 再悬停普通根项 → 关闭。构造双 flyout 根项菜单验证行为表的「切换」行。
    #[gpui::test]
    fn flyout_switches_between_flyout_roots(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub_a = vec![
                    PopupMenuItem::element(|_, _| {
                        div().debug_selector(|| "fly-a1".into()).child("默认").into_any_element()
                    })
                    .keep_open()
                    .on_click(|_, _, _| {}),
                    PopupMenuItem::element(|_, _| {
                        div().debug_selector(|| "fly-a2".into()).child("夜间").into_any_element()
                    })
                    .keep_open()
                    .on_click(|_, _, _| {}),
                ];
                let sub_b = vec![PopupMenuItem::element(|_, _| {
                    div().debug_selector(|| "fly-b1".into()).child("形状一").into_any_element()
                })
                .keep_open()
                .on_click(|_, _, _| {})];
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    menu.item(
                        PopupMenuItem::element(|_, _| {
                            div().debug_selector(|| "root-bg".into()).child("背景色").into_any_element()
                        })
                        .flyout(sub_a),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div().debug_selector(|| "root-shape".into()).child("形状").into_any_element()
                        })
                        .flyout(sub_b),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div().debug_selector(|| "root-insert".into()).child("插入项").into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                });
                menu.read(cx).focus_handle(cx).focus(window, cx);
                menu.update(cx, |m, _| m.set_parent_pos(point(px(0.), px(0.))));
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // 悬停背景色 → A 次级展开，B 不在场。
        hover_center(cx, "root-bg");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停背景色应展开次级");
        assert!(cx.debug_bounds("fly-a1").is_some(), "A 次级应在场");
        assert!(cx.debug_bounds("fly-b1").is_none(), "B 次级不应在场");

        // 悬停另一个带 flyout 的根项 → 次级**切换**为 B（A 关、B 开）。
        hover_center(cx, "root-shape");
        assert!(cx.debug_bounds("fly-b1").is_some(), "悬停形状应切换为 B 次级");
        assert!(cx.debug_bounds("fly-a1").is_none(), "切走后 A 次级应关闭");

        // 切回背景色 → 恢复 A。
        hover_center(cx, "root-bg");
        assert!(cx.debug_bounds("fly-a1").is_some(), "切回背景色应恢复 A 次级");
        assert!(cx.debug_bounds("fly-b1").is_none(), "B 次级应关闭");

        // 悬停普通根项 → 关闭次级（行为表：移动到其他选项）；仅悬停不触发整链 dismiss。
        hover_center(cx, "root-insert");
        assert!(cx.debug_bounds("menu-flyout").is_none(), "悬停普通根项应关闭次级");
        assert_eq!(dismisses.get(), 0, "仅悬停移动不应触发整链 dismiss");
    }

    /// 几何：次级菜单顶部 ≈ 背景色行顶部；左缘 ≈ 父菜单右缘（容差 FLYOUT_GEOM_TOL 内）。
    #[gpui::test]
    fn flyout_geometry_aligned_right_of_parent(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = flyout_harness(cx);
        hover_center(cx, "root-bg");

        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("root-bg").expect("背景色行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父菜单应在场");
        // 顶部 ≈ 背景色行顶部（允许越界 ≤ 容差）。
        let dy = (fly.top() - row.top()).abs();
        assert!(dy <= px(FLYOUT_GEOM_TOL), "次级顶部应≈背景色行顶部，实际差 {dy:?}");
        // 左缘 ≈ 父菜单右缘（允许左移 ≤ 容差；次级在父菜单右侧而不是盖在父菜单上）。
        let dx = fly.left() - panel.right();
        assert!(
            dx >= -px(FLYOUT_GEOM_TOL) && dx <= px(40.),
            "次级左缘应贴着父菜单右缘，实际偏移 {dx:?}"
        );
        // 次级面板与父面板水平不重叠（左缘 ≥ 父右缘 - 容差即在右邻）。
        assert!(
            fly.right() > panel.right(),
            "次级面板应在父菜单右缘右侧"
        );
    }

    /// shell 宿主 `deferred(anchored(Window)).position(pos)` 下 flyout 几何与 flex 宿主一致
    /// （anchored 子元素按内容宽布局，不把菜单拉伸到全窗宽）。
    #[gpui::test]
    fn flyout_geometry_under_anchored_window_host(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let clicks = Rc::new(Cell::new(0usize));
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub_click = clicks.clone();
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    let sub = vec![
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .debug_selector(|| "flyout-a".into())
                                .child("默认")
                                .into_any_element()
                        })
                        .keep_open()
                        .on_click(move |_, _, _| sub_click.set(sub_click.get() + 1)),
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "flyout-b".into())
                                .child("夜间")
                                .into_any_element()
                        })
                        .keep_open()
                        .on_click(|_, _, _| {}),
                    ];
                    menu.item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-close".into())
                                .child("关闭项")
                                .into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-bg".into())
                                .child("背景色")
                                .into_any_element()
                        })
                        .flyout(sub),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-insert".into())
                                .child("插入项")
                                .into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                });
                menu.read(cx).focus_handle(cx).focus(window, cx);
                // 写入父菜单锚点 = anchored(Window) 的请求位 (50,50)，镜像 shell 语义。
                menu.update(cx, |m, _| m.set_parent_pos(point(px(50.), px(50.))));
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                AnchoredMenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        hover_center(cx, "root-bg");

        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("root-bg").expect("背景色行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父菜单应在场");
        // 菜单经 anchored(Window) 定位在 (50,50)（snap 后仍在左上），面板不应被拉伸成全窗宽。
        assert!(
            panel.size.width < px(250.),
            "anchored 宿主下面板不应拉伸到全窗宽，实际宽 {:?}",
            panel.size.width
        );
        let dy = (fly.top() - row.top()).abs();
        assert!(dy <= px(FLYOUT_GEOM_TOL), "anchored 下顶部应≈根项行顶，实际差 {dy:?}");
        let dx = fly.left() - panel.right();
        assert!(
            dx >= -px(FLYOUT_GEOM_TOL) && dx <= px(40.),
            "anchored 下左缘应≈父菜单右缘，实际偏移 {dx:?}"
        );
    }

    #[gpui::test]
    fn flyout_click_preset_executes_and_keeps_open(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, clicks, dismisses) = flyout_harness(cx);
        hover_center(cx, "root-bg");
        // 先移入次级子项再点（真实路径：悬停守卫新鲜，点次级面板不会触发「面板外」关闭）。
        hover_center(cx, "flyout-a");
        let sub = cx.debug_bounds("flyout-a").expect("次级默认项应在场").center();
        cx.simulate_click(sub, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "点击次级预设应执行回调");
        assert_eq!(dismisses.get(), 0, "keep_open 次级项不应关闭菜单");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "keep_open 下次级面板保持展开");
        assert!(cx.debug_bounds("root-bg").is_some(), "父菜单保持展开");
    }

    /// 非 keep_open 的 flyout 子项点击 → 执行回调后**整链关闭**（次级 + 父菜单，
    /// dismiss=1、二级面板消失）——「打开最近」flyout 子项（打开路径/清空）即此形态：
    /// 点击后整链关 + 动作生效，与 PDF 背景色（子项全 keep_open）语义对照。
    /// keep_open 分支已由 [`flyout_click_preset_executes_and_keeps_open`] 覆盖。
    #[gpui::test]
    fn flyout_click_item_dismisses_chain(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let clicks = Rc::new(Cell::new(0usize));
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub_click = clicks.clone();
                let sub = vec![PopupMenuItem::element(move |_, _| {
                    div()
                        .debug_selector(|| "flyout-open".into())
                        .child("打开文件")
                        .into_any_element()
                })
                .on_click(move |_, _, _| sub_click.set(sub_click.get() + 1))];
                let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
                    menu.item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-open".into())
                                .child("打开最近…")
                                .into_any_element()
                        })
                        .flyout(sub),
                    )
                    .item(
                        PopupMenuItem::element(|_, _| {
                            div()
                                .debug_selector(|| "root-close".into())
                                .child("关闭项")
                                .into_any_element()
                        })
                        .on_click(|_, _, _| {}),
                    )
                });
                menu.read(cx).focus_handle(cx).focus(window, cx);
                menu.update(cx, |m, _| m.set_parent_pos(point(px(0.), px(0.))));
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        // 悬停 flyout 根项 → 次级展开。
        hover_center(cx, "root-open");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停应展开次级");
        // 移入次级子项再点（悬停守卫新鲜，不误触「面板外」关闭）→ 回调执行 + 整链关闭。
        hover_center(cx, "flyout-open");
        let sub = cx.debug_bounds("flyout-open").expect("子项应在场").center();
        cx.simulate_click(sub, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(clicks.get(), 1, "点击非 keep_open 子项应执行回调");
        assert_eq!(dismisses.get(), 1, "点击非 keep_open 子项应整链关闭（dismiss=1）");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("menu-flyout").is_none(), "dismiss 后次级菜单应消失");
    }

    #[gpui::test]
    fn flyout_escape_closes_submenu_then_parent(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, dismisses) = flyout_harness(cx);
        hover_center(cx, "root-bg");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停应展开次级");
        // 首次 Esc：只关次级、父菜单保持展开（不 dismiss）。
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("menu-flyout").is_none(), "首次 Esc 应关次级");
        assert!(cx.debug_bounds("root-bg").is_some(), "首次 Esc 父菜单应保持");
        assert_eq!(dismisses.get(), 0, "首次 Esc 不应触发整链 dismiss");
        // 再次 Esc：关闭父菜单（dismiss=1）。
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 1, "再次 Esc 应触发整链 dismiss");
        // 父面板消失属宿主槽位清理职责，此处以 dismiss 计数为准（MenuHost 不摘实体）。
    }

    #[gpui::test]
    fn flyout_click_outside_dismisses_all(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, dismisses) = flyout_harness(cx);
        hover_center(cx, "root-bg");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停应展开次级");
        // 点父菜单/次级之外的空白 → 整链关闭。
        let outside = {
            let panel = cx.debug_bounds("popup-menu").expect("父菜单应在场");
            let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
            let right = panel.right().max(fly.right()) + px(40.);
            gpui::point(right, panel.top() + px(10.))
        };
        cx.simulate_click(outside, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 1, "点面板外应关闭整链");
        // dismiss 不主动 notify（实时序靠宿主清槽位重绘）；测试宿主无清理 → 强制 draw 一帧。
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("menu-flyout").is_none(), "次级应消失");
        // 父面板消失属宿主槽位清理职责（shell spawn_context_menu 清 slot），此处以 dismiss 为准。
    }

    /// 建「镜像 PDF 右键排列」菜单：普通项/分割线/普通项×3/分割线/背景色(flyout×5)。
    /// 「背景色」根行位于两条分割线之间、下标 6（与真实 PDF 右键排列一致——真实场景
    /// 为 添加书签/分割线/插入/删除/提取/分割线/背景色），用于校验 flyout 顶部对齐
    /// 必须按真实行高（分割线 ≠ ITEM_HEIGHT）累计，以及父面板不被次级 `mt` 撑高成
    /// 「点下方空白不关闭」的死区。
    fn flyout_mirror(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<Cell<usize>>) {
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let dismisses = dismisses.clone();
            move |window, cx| {
                let sub: Vec<PopupMenuItem> = (0..5)
                    .map(|k| {
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .debug_selector(move || format!("m-fly-{k}"))
                                .child("预设")
                                .into_any_element()
                        })
                        .keep_open()
                        .on_click(|_, _, _| {})
                    })
                    .collect();
                let root = |tag: &'static str| {
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .debug_selector(move || tag.into())
                            .child(tag)
                            .into_any_element()
                    })
                };
                let menu = PopupMenu::build(window, cx, move |menu, _w, _cx| {
                    menu.item(root("m-r0"))
                        .separator()
                        .item(root("m-r1"))
                        .item(root("m-r2"))
                        .item(root("m-r3"))
                        .separator()
                        .item(root("m-bg").flyout(sub))
                });
                menu.read(cx).focus_handle(cx).focus(window, cx);
                menu.update(cx, |m, _| m.set_parent_pos(point(px(0.), px(0.))));
                let d = dismisses.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                        d.set(d.get() + 1);
                    })
                    .detach();
                MenuHost { menu }
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, dismisses)
    }

    /// 父菜单**内含分割线**时，次级顶部仍须 ≈ 背景色根行顶部（分割线行高 < ITEM_HEIGHT，
    /// 对齐偏移若按「每项都 26px」算会把次级压到根行下方十几~几十像素）。
    #[gpui::test]
    fn flyout_geometry_with_separators_aligns_to_root_row(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _) = flyout_mirror(cx);
        hover_center(cx, "m-bg");

        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("m-bg").expect("背景色行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父菜单应在场");
        let dy = (fly.top() - row.top()).abs();
        assert!(
            dy <= px(FLYOUT_GEOM_TOL),
            "父内含分割线时次级顶部应≈背景色行顶部，实际差 {dy:?}"
        );
        let dx = fly.left() - panel.right();
        assert!(
            dx >= -px(FLYOUT_GEOM_TOL) && dx <= px(12.),
            "分隔线场景下次级左缘应贴父右缘，实际偏移 {dx:?}"
        );
        assert!(fly.right() > panel.right(), "次级应在父菜单右缘右侧");
    }

    /// 死区/撑高守卫：次级 `mt` 不应把**父菜单面板**拉长——flex 默认 cross-axis stretch
    /// 会把父面板高度拉伸到「次级底部」（真实 PDF 排列下 182→341px），变成巨大空丙烯
    /// 大尾巴且观感异常；另验证「父菜单下方空白」点击仍整链关闭（真实用户动作）。
    #[gpui::test]
    fn flyout_parent_panel_not_inflated_and_click_below_closes(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, dismisses) = flyout_mirror(cx);
        hover_center(cx, "m-bg");
        assert!(cx.debug_bounds("menu-flyout").is_some(), "悬停应展开次级");

        let panel = cx.debug_bounds("menu-panel").expect("父菜单应在场");
        let row = cx.debug_bounds("m-bg").expect("背景色行应在场");
        assert!(
            panel.size.height <= px(200.),
            "父面板不应被次级撑高成空白大尾巴，实际高 {:?}（内容约 182px）",
            panel.size.height
        );

        let below = gpui::point(row.left() + px(10.), row.bottom() + px(60.));
        cx.simulate_mouse_move(below, None, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(below, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(dismisses.get(), 1, "点父菜单下方空白应整链关闭");
    }

    // ---------------------------------------------------------------------
    // 窗口边缘空间适配（本轮）：flyout 方向决策纯函数 + 近底部向上翻/近右缘向左翻 +
    // 连续帧稳定性（无振荡闪滑）。host 镜像 shell `deferred(anchored(Window)).position(pos)`
    // + snap 语义，`parent_pos` 同步为 pos。
    // ---------------------------------------------------------------------

    /// 模拟 shell 锚定的边缘宿主：菜单经 `anchored(Window).position(pos)` + snap
    /// （与 `render_pdf_menu` 同语义）挂载，窗口尺寸由测试 `simulate_resize` 设定。
    struct EdgeMenuHost {
        menu: Entity<PopupMenu>,
        pos: gpui::Point<gpui::Pixels>,
    }

    impl Render for EdgeMenuHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("edge-host")
                .size_full()
                .child(
                    deferred(
                        anchored()
                            .position_mode(gpui::AnchoredPositionMode::Window)
                            .anchor(Anchor::TopLeft)
                            .position(self.pos)
                            .snap_to_window_with_margin(Edges::all(px(8.)))
                            .child(self.menu.clone()),
                    )
                    .with_priority(100),
                )
        }
    }

    /// 三根项（关闭/背景色 flyout/插入）+ 二级两预设 的菜单实体装配（含锚点写入）。
    fn edge_menu(
        window: &mut Window,
        cx: &mut Context<EdgeMenuHost>,
        pos: gpui::Point<gpui::Pixels>,
        clicks: Rc<Cell<usize>>,
        dismisses: Rc<Cell<usize>>,
    ) -> Entity<PopupMenu> {
        let sub_click = clicks.clone();
        let menu = PopupMenu::build(window, cx, |menu, _w, _cx| {
            let sub = vec![
                PopupMenuItem::element(move |_, _| {
                    div()
                        .debug_selector(|| "flyout-a".into())
                        .child("默认")
                        .into_any_element()
                })
                .keep_open()
                .on_click(move |_, _, _| sub_click.set(sub_click.get() + 1)),
                PopupMenuItem::element(|_, _| {
                    div()
                        .debug_selector(|| "flyout-b".into())
                        .child("夜间")
                        .into_any_element()
                })
                .keep_open()
                .on_click(|_, _, _| {}),
            ];
            menu.item(
                PopupMenuItem::element(|_, _| {
                    div()
                        .debug_selector(|| "root-close".into())
                        .child("关闭项")
                        .into_any_element()
                })
                .on_click(|_, _, _| {}),
            )
            .item(
                PopupMenuItem::element(|_, _| {
                    div()
                        .debug_selector(|| "root-bg".into())
                        .child("背景色")
                        .into_any_element()
                })
                .flyout(sub),
            )
            .item(
                PopupMenuItem::element(|_, _| {
                    div()
                        .debug_selector(|| "root-insert".into())
                        .child("插入项")
                        .into_any_element()
                })
                .on_click(|_, _, _| {}),
            )
        });
        menu.read(cx).focus_handle(cx).focus(window, cx);
        menu.update(cx, |m, _| m.set_parent_pos(pos));
        let d = dismisses.clone();
        window
            .subscribe(&menu, cx, move |_, _: &DismissEvent, _, _| {
                d.set(d.get() + 1);
            })
            .detach();
        menu
    }

    /// 建边缘宿主（窗口 resize 到 `win`、菜单请求锚点 `pos`）。
    fn edge_harness(
        cx: &mut TestAppContext,
        win: gpui::Size<f32>,
        pos: gpui::Point<f32>,
    ) -> (&mut VisualTestContext, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        let clicks = Rc::new(Cell::new(0usize));
        let dismisses = Rc::new(Cell::new(0usize));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let dismisses = dismisses.clone();
            move |window, cx| {
                let menu = edge_menu(
                    window,
                    cx,
                    gpui::point(px(pos.x), px(pos.y)),
                    clicks,
                    dismisses,
                );
                EdgeMenuHost {
                    menu,
                    pos: gpui::point(px(pos.x), px(pos.y)),
                }
            }
        });
        cx.simulate_resize(gpui::size(px(win.width), px(win.height)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, clicks, dismisses)
    }

    /// 连续 draw `n` 帧、采集「次级 + 父面板」边界，用于断言无闪烁（帧间完全一致）。
    fn sample_stable(
        cx: &mut VisualTestContext,
        n: usize,
    ) -> Vec<(Option<gpui::Bounds<gpui::Pixels>>, Option<gpui::Bounds<gpui::Pixels>>)> {
        let mut out = Vec::new();
        for _ in 0..n {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            out.push((cx.debug_bounds("menu-flyout"), cx.debug_bounds("menu-panel")));
        }
        out
    }

    fn assert_frames_identical(frames: &[(Option<gpui::Bounds<gpui::Pixels>>, Option<gpui::Bounds<gpui::Pixels>>)]) {
        assert!(
            frames.windows(2).all(|w| w[0] == w[1]),
            "连续帧边界应完全一致（无闪烁/振荡），帧序列: {frames:?}"
        );
    }

    /// 方向决策纯函数：中部 → Right/Down；近底 → Up；近右 → Left；双近边 → Left+Up；
    /// 边界「恰好放得下」→ 不翻转（≤ 判定，与现状一致）。
    #[gpui::test]
    fn flyout_direction_pure_function(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (win, row_top, sub_w, sub_h, margin) =
            (gpui::size(px(800.), px(600.)), px(35.), px(160.), px(66.), px(8.));
        let panel = |x: f32, y: f32| {
            gpui::Bounds::new(gpui::point(px(x), px(y)), gpui::size(px(160.), px(96.)))
        };
        // 中部：左右/上下都够 → Right/Down（回归现状）。
        assert_eq!(
            flyout_direction(win, panel(300., 200.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Right, FlyoutV::Down)
        );
        // 近底部（y 大）：垂直向上翻、水平仍右。
        assert_eq!(
            flyout_direction(win, panel(300., 500.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Right, FlyoutV::Up)
        );
        // 近右缘（x 大）：水平向左翻、垂直仍下。
        assert_eq!(
            flyout_direction(win, panel(620., 200.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Left, FlyoutV::Down)
        );
        // 右下角：双翻。
        assert_eq!(
            flyout_direction(win, panel(620., 500.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Left, FlyoutV::Up)
        );
        // 边界「恰好放得下」= 不翻转（≤ 判定）。
        assert_eq!(
            flyout_direction(win, panel(800. - 8. - 160. - 160. - 1., 200.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Right, FlyoutV::Down)
        );
        assert_eq!(
            flyout_direction(win, panel(300., 600. - 8. - 66. - 35. - 1.), row_top, sub_w, sub_h, margin),
            (FlyoutH::Right, FlyoutV::Down)
        );
    }

    /// `flyout_rect` 自夹持：方向 + 最终矩形恒定在窗口内（含窄窗内兜底 clamp）。
    #[gpui::test]
    fn flyout_rect_clamps_inside_window(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        // 近右缘且右侧放不下 → Left；左缘不够再 clamp 到留白。
        let win = gpui::size(px(300.), px(200.));
        let panel = gpui::Bounds::new(gpui::point(px(16.), px(120.)), gpui::size(px(160.), px(96.)));
        let (h, v, pos) = flyout_rect(win, panel, px(35.), px(160.), px(66.), px(8.));
        assert_eq!((h, v), (FlyoutH::Left, FlyoutV::Up));
        assert_eq!(pos.x, px(8.), "Left 候选越左缘应 clamp 到留白");
        assert!(pos.y >= px(8.) && pos.y + px(66.) <= px(200.) - px(8.), "垂直应完整在窗内");
    }

    /// 近底部：次级**向上翻**（top < 根行 top），父/次级完整在窗内，连续 ≥6 帧完全一致。
    #[gpui::test]
    fn flyout_flips_up_near_bottom_and_stable(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = edge_harness(cx, gpui::size(400., 300.), gpui::point(20., 250.));
        // 根因取证：flyout 未开时先记录父面板边界；展开后父面板**不得被 re-pin**（旧
        // 设计中次级回流把 union 撑高 → shell snap 回摆父面板 → hover 抖动 = 闪滑源）。
        let panel_before = cx.debug_bounds("menu-panel").expect("父面板应在场");
        hover_center(cx, "root-bg");
        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("root-bg").expect("背景色行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父面板应在场");
        assert_eq!(panel_before, panel, "展开次级不应 re-pin 父面板（根因：次级已脱离 union 流）");
        assert!(
            fly.top() < row.top(),
            "近底部应向上翻（次级顶 {fly:?} 应在根行顶之上）"
        );
        // 父/次级完整落在窗口内（300 高 - 边缘留白 8）。
        assert!(panel.top() >= px(8.) && panel.bottom() <= px(300.) - px(8.), "父面板应在窗口内");
        assert!(fly.top() >= px(8.) && fly.bottom() <= px(300.) - px(8.), "次级应在窗口内");
        // 稳定性：连续 6 帧边界完全一致（无闪滑）。
        let frames = sample_stable(cx, 6);
        assert_frames_identical(&frames);
    }

    /// 近右缘：次级**翻到父菜单左邻**（次级右缘 ≈ 父左缘），顶部仍对根项行，多帧稳定。
    #[gpui::test]
    fn flyout_flips_left_near_right_edge_and_stable(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = edge_harness(cx, gpui::size(400., 300.), gpui::point(230., 20.));
        hover_center(cx, "root-bg");
        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let row = cx.debug_bounds("root-bg").expect("背景色行应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父面板应在场");
        // 次级右缘 ≈ 父左缘（左翻）。
        let gap = (fly.right() - panel.left()).abs();
        assert!(gap <= px(FLYOUT_GEOM_TOL), "次级右缘应≈父左缘，实际差 {gap:?}");
        // 顶部仍对齐根项行；父面板不被异常挤动（其左缘仍在请求锚点附近）。
        let dy = (fly.top() - row.top()).abs();
        assert!(dy <= px(FLYOUT_GEOM_TOL), "次级顶部应≈背景色行顶，实际差 {dy:?}");
        assert!(fly.left() >= px(8.), "次级应在窗口内（左缘 {fly:?}）");
        assert!((panel.left() - px(230.)).abs() <= px(FLYOUT_GEOM_TOL), "父面板不应被挤动");
        // 稳定性。
        let frames = sample_stable(cx, 6);
        assert_frames_identical(&frames);
    }

    /// 小窗口 300×200：父+次级完整在窗内、连续帧稳定（可能重叠但不得越窗/闪滑）。
    #[gpui::test]
    fn flyout_fits_small_window_stable(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (cx, _, _) = edge_harness(cx, gpui::size(300., 200.), gpui::point(150., 120.));
        hover_center(cx, "root-bg");
        let fly = cx.debug_bounds("menu-flyout").expect("次级应在场");
        let panel = cx.debug_bounds("menu-panel").expect("父面板应在场");
        let in_win = |b: gpui::Bounds<gpui::Pixels>| {
            b.left() >= px(0.) && b.top() >= px(0.) && b.right() <= px(300.) && b.bottom() <= px(200.)
        };
        assert!(in_win(panel), "父面板应完整在窗内，实际 {panel:?}");
        assert!(in_win(fly), "次级应完整在窗内，实际 {fly:?}");
        let frames = sample_stable(cx, 6);
        assert_frames_identical(&frames);
    }
}
