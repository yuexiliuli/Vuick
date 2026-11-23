//! 统一「图标按钮」：全屏/右栏/底栏三处图标钮机制与观感收口（第 1/4 步：新建 + 右栏迁移）。
//!
//! - hover 高亮 = `bg_hover().opacity(0.6)` 圆形淡底 + 图标前色 normal→hover（默认 text2→text）；
//!   active 态语义保留（like pink / shuffle・repeat accent_light，[`resolve_fg`] 纯函数），
//!   hover **不覆盖** active（active 优先）。
//! - 悬停名称提示走调用方注入 `tip_writer`，浮层复用父视图 `music-tooltip` 单一层
//!   （[`MusicView::hover_tip_listener`] 工厂注入；本组件不新建浮层、不持有视图字段）。
//!   锚定盒经 [`theme::bounds_canvas`] 经 `cell: Rc<Cell<Bounds<Pixels>>>` 写调用方
//!   `tip_box_cell(id)` 记录的 cell（`absolute().inset_0()` 铺满本钮，prepaint 写入窗口盒）。
//! - 主播放钮经 `fill_bg = Some(底元素)` 保留实心 accent 圆：底恒调用方元素、hover 叠白 %12
//!   覆盖层；`fill_bg = None` 时 hover 圆形淡底 / 未悬停透明。
//! - `click` / `tip_writer` / `cell` 全注入 → `MusicView` 与 `BottomBarView` 两上下文同签名
//!   消费（`Context<V>` 泛型）；hover 瞬时态亦泛型化为 [`hover_state`]，不新增任何视图字段。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};

use super::theme;
use super::IntoIconElement;

/// 元素级 hover 瞬时态（无动画、self-contained）：复用 titlebar `ctrl_button` 已验证的
/// `window.use_keyed_state(key, cx, |_, _| false)` 姿势——不新增视图字段、不改 mod.rs。
/// 泛型 `V` = 调用方视图（MusicView / BottomBarView…）：写回/通知经 window 的当前视图 id，
/// 与调用方上下文类型无关。返回 `(状态句柄, 当前是否悬停)`；句柄 `.clone()` 交给 `.on_hover`
/// （[`hover_listener`]）。`key` 取元素唯一标识（按钮 label/行下标…），多实例元素各自独立互不覆盖。
pub(super) fn hover_state<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    key: String,
) -> (Entity<bool>, bool) {
    #[cfg(test)]
    let reg_key = key.clone();
    let st = window.use_keyed_state(key, cx, |_, _| false);
    #[cfg(test)]
    hover_registry().lock().unwrap().insert(reg_key, st.clone());
    let hovered = *st.read(cx);
    (st, hovered)
}

/// 由 hover 状态句柄构造 `.on_hover` 监听：进入/离开写回布尔 + 显式重绘当前视图
/// （对齐 titlebar `ctrl_button` 双重通知姿势：`observe_keyed_state` 观察 + `cx.notify`）。
/// 注意：同一元素已有 tooltip `on_hover`（tab/附加钮）时不用本函数，改用「组合监听」
/// （先取 `hover_tip_listener` 闭包，再与本函数写回逻辑合并在一个 `.on_hover` 里——
/// [`icon_btn`] 内部即此姿势）。
pub(super) fn hover_listener(
    window: &mut Window,
    st: &Entity<bool>,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let st = st.clone();
    let view_id = window.current_view();
    move |hovered, _w, cx| {
        st.update(cx, |h, _| *h = *hovered);
        cx.notify(view_id);
    }
}

/// 测试只读通道：`hover_state` 把创建的 keyed 状态句柄登记到此注册表，测试经
/// [`hover_read`] 读取「悬停后状态翻转」。判别式：撤掉元素 `.on_hover` 的状态写回 → 恒 false → 测试红。
#[cfg(test)]
fn hover_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, Entity<bool>>> {
    use std::sync::LazyLock;
    static REG: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Entity<bool>>>> =
        LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    &REG
}

/// 读某元素当前 hover 状态（未登记 → false）。测试在「先渲染一帧再读」后调用，
/// 此时注册表已被本测试上下文内最新渲染刷新，不会读到异构上下文的悬空句柄。
#[cfg(test)]
pub(super) fn hover_read(key: &str, cx: &App) -> bool {
    match hover_registry().lock().unwrap().get(key) {
        Some(st) => *st.read(cx),
        None => false,
    }
}

/// 图标前色解析：**active 优先**——hover 不覆盖 active 语义色（如 like pink /
/// shuffle・repeat accent_light）；active 且未给 active 色 → 回落 hover 色。纯函数，单独可单测。
pub(super) fn resolve_fg(
    active: bool,
    hovered: bool,
    normal: gpui::Hsla,
    hover: gpui::Hsla,
    active_c: Option<gpui::Hsla>,
) -> gpui::Hsla {
    if active {
        active_c.unwrap_or(hover)
    } else if hovered {
        hover
    } else {
        normal
    }
}

/// 统一「图标按钮」。
///
/// - `id` / `hover_key`：元素唯一标识 / keyed hover 状态键（二者独立：多个调用方可共用
///   hover 键从而共享同一悬停态，`.id()` 仍各自独立）。
/// - `icon` / `icon_size` / `box_size` / `radius`：图标（经 [`IntoIconElement`] 着色缩放）、
///   图标边长、钮盒边长、圆角。
/// - `label = Some(text)`：**药丸形态**（icon + gap5 + 文本，如附加钮现样式），宽随内容；
///   `None`：纯图标圆形钮（`box_size × box_size` 定宽高）。
/// - `active` / `fg_normal` / `fg_hover` / `fg_active`：[`resolve_fg`] active 优先解析前色。
/// - `fill_bg = None`：hover 圆形淡底 `bg_hover().opacity(0.6)` / 未悬停透明；
///   `Some(bg_elem)`：底恒 `bg_elem`（调用方自备 absolute 铺底元素，如 accent 圆），
///   hover 另叠 `white_fade_12()` 覆盖层（white 淡显、不盖 active 语义）。
/// - `cell`：tooltip 锚定盒（[`theme::bounds_canvas`] 铺满本钮写窗口盒）。
/// - `tip_writer`：悬停名称提示监听（父 `music-tooltip` 单一层）；与 hover 写回合并在同一
///   `.on_hover`（对齐 right `extra_btn` 现组合姿势）。
/// - `on_click`：点击处理（含 `&mut Window`，如分享需写窗）。
pub(super) fn icon_btn<V: 'static, I: IntoIconElement>(
    window: &mut Window,
    cx: &mut Context<V>,
    id: &'static str,
    hover_key: String,
    icon: I,
    icon_size: f32,
    box_size: f32,
    radius: f32,
    label: Option<&'static str>,
    active: bool,
    fg_normal: gpui::Hsla,
    fg_hover: gpui::Hsla,
    fg_active: Option<gpui::Hsla>,
    fill_bg: Option<AnyElement>,
    cell: Rc<Cell<Bounds<Pixels>>>,
    tip_writer: impl Fn(&bool, &mut Window, &mut App) + 'static,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (st, hovered) = hover_state(window, cx, hover_key);
    let fg = resolve_fg(active, hovered, fg_normal, fg_hover, fg_active);

    let mut children: Vec<AnyElement> = Vec::with_capacity(4);
    // 背景层：fill_bg=None → hover 圆形淡底 / 透明；Some → 底恒调用方元素、hover 叠白淡显。
    match fill_bg {
        Some(bg_elem) => {
            children.push(bg_elem);
            if hovered {
                children.push(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(px(radius))
                        .bg(theme::white_fade_12())
                        .into_any_element(),
                );
            }
        }
        None => {
            children.push(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(radius))
                    .bg(if hovered {
                        theme::bg_hover().opacity(0.6)
                    } else {
                        gpui::transparent_black()
                    })
                    .into_any_element(),
            );
        }
    }
    // 图标（已注入 resolved 前色）。
    children.push(icon.icon_element(px(icon_size), fg));
    // 药丸形态：icon + gap + 文本（文本随容器 `text_size`/`text_color`）。
    if let Some(text) = label {
        children.push(
            div()
                .text_size(px(12.))
                .text_color(fg)
                .child(text.to_string())
                .into_any_element(),
        );
    }
    // tooltip 锚定盒：absolute 铺满本钮，prepaint 记录窗口盒给父 `music-tooltip` 单一层。
    children.push(theme::bounds_canvas(cell));

    // 组合 on_hover：写 keyed hover + notify + tip_writer（对齐 extra_btn 现组合姿势）。
    let st_for_hover = st.clone();
    let view_id = window.current_view();
    let on_hover = move |hovered: &bool, w: &mut Window, cx: &mut App| {
        st_for_hover.update(cx, |h, _| *h = *hovered);
        cx.notify(view_id);
        tip_writer(hovered, w, cx);
    };

    let mut el = div()
        .id(id)
        .debug_selector(move || id.into())
        .rounded(px(radius))
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .cursor(CursorStyle::PointingHand)
        .on_click(on_click)
        .on_hover(on_hover);
    // 纯图标钮：定宽高正方盒（圆形钮）；药丸形态：宽随内容、悬停淡底仍铺满内容盒。
    match label {
        None => el = el.w(px(box_size)).h(px(box_size)),
        Some(_) => el = el.gap_x(px(5.)),
    }
    el.children(children).into_any_element()
}
