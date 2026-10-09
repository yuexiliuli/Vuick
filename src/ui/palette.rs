//! 命令面板（P2-8，VSCode 标志性功能）：Ctrl+Shift+P 命令 / Ctrl+P 文件快选两用浮层。
//!
//! 居中的 Acrylic 浮层：顶部查询框（复用 [`InputState`] 的 IME/编辑/退格），下列过滤后的
//! 条目——命令模式列全部命令（带分类分组标签 + 快捷键提示），文件模式列已打开文件
//! 以及「打开文件…（系统对话框）」入口。↑↓ 选行、Enter 执行、Esc / 点击面板外关闭，
//! 并把焦点还给打开面板前的控件（与 [`super::menu::PopupMenu`] 同一套归还模式）。
//!
//! # 生命周期与性能
//! 面板是**独立实体**（`query` / `selected` / 模式 / 条目列表），由宿主 App 在每次打开时
//! `cx.new(...)` 创建、`Option` 持有；关闭（[`DismissEvent`]）后宿主清引用、实体随之释放。
//! 过滤是纯字符串操作（命令 ≤ 百余条、文件 = 已打开数，纳秒级），每次输入变化重算，
//! 无缓存/后台线程；`with_spring` 打开淡入收敛即静止，关闭后不再有逐帧绘制。
//!
//! # 焦点与键盘
//! 打开时聚焦查询输入框（文本自然流入 [`InputState`] 的 `Input` 键上下文；其未处理的
//! ↑↓/Enter/Esc 原始按键沿元素树冒泡到本面板根的 `on_key_down` 处理）。

use std::rc::Rc;

use gpui::{
    AnimationExt, AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseDownEvent, ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};

use crate::ui::input::{Input, InputEvent, InputState};
use crate::ui::motion::hover_spring;
use crate::ui::scrollbar::{Scrollbar, ScrollbarMode};
use crate::ui::theme::{ChromeColors, FLOAT_ACRYLIC_ALPHA_DARK, FLOAT_ACRYLIC_ALPHA_LIGHT, Theme};

/// 面板模式（创建时定死；Ctrl+Shift+P / Ctrl+P 各自新建一个面板实体）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaletteMode {
    /// 命令模式（Ctrl+Shift+P）：全部命令。
    Commands,
    /// 文件模式（Ctrl+P）：已打开文件快选。
    Files,
}

/// 条目执行器：确认时（Enter / 点击）运行，与 `PopupMenuItem` 的处理器同型。
type PaletteRunner = Rc<dyn Fn(&mut Window, &mut App)>;

/// 面板条目：左主文案（命令名 / 文件名）+ 右次要文案（分类 / 路径）+ 快捷键提示。
pub struct PaletteItem {
    /// 主文案（命令名 / 文件名），参与过滤。
    pub label: String,
    /// 次要文案：命令模式 = 分类分组标签；文件模式 = 路径提示。
    pub detail: String,
    /// 右侧快捷键提示（空串不显示）。
    pub shortcut: String,
    /// 确认时执行（Enter / 点击）。
    pub run: PaletteRunner,
}

impl PaletteItem {
    /// 便捷构造（宿主组装命令 / 文件条目用）。
    pub fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        shortcut: impl Into<String>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            shortcut: shortcut.into(),
            run: Rc::new(run),
        }
    }
}

/// 过滤得分：越大越靠前；`None` = 不匹配。
///
/// 大小写不敏感子串/前缀 + 简单模糊（匹配字符按顺序跨间隔出现的子序列）：
/// 前缀命中 > 子串命中 > 乱序子序列；得分相同时保持原顺序（调用方稳定排序）。
fn fuzzy_score(query: &str, label: &str) -> Option<u32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let l = label.to_lowercase();
    if l.starts_with(&q) {
        // 前缀命中：越贴近查询越靠前（弱化长标签）。
        return Some(1000 + (l.len() as u32).saturating_sub(q.len() as u32));
    }
    if let Some(pos) = l.find(q.as_str()) {
        // 子串命中：命中的位置越靠前越好。
        return Some(500 + (l.len() as u32).saturating_sub((pos + q.len()) as u32));
    }
    // 简单模糊：查询字符按顺序在标签中出现（允许间隔），间隔越小得分越高。
    let mut qi = q.chars().peekable();
    let mut score: u32 = 200;
    let mut complete = false;
    for ch in l.chars() {
        if qi.peek() == Some(&ch) {
            qi.next();
            score += 4;
            if qi.peek().is_none() {
                complete = true;
                break;
            }
        } else if score > 0 {
            // 间隔惩罚：已用 `score > 0` 门控，饱和减与「不减到负」等价（clippy 建议写法）。
            score = score.saturating_sub(1);
        }
    }
    if complete {
        Some(score)
    } else {
        None
    }
}

/// 条目行高。
const ITEM_HEIGHT: f32 = 30.;

/// 居中的命令面板视图（独立实体，宿主持有 `Option<Entity<Self>>`）。
pub struct CommandPalette {
    mode: PaletteMode,
    /// 顶部查询输入框（IME/退格/光标由 [`InputState`] 自理，自身不做文本处理）。
    input: Entity<InputState>,
    /// 输入变化 → 重算过滤 + 复位选中。
    _input_sub: gpui::Subscription,
    /// 全部候选条目（宿主打开时快照；文件模式含顶部「打开文件…」固定入口。
    /// 命令模式全部命令；均带执行回调）。
    items: Vec<PaletteItem>,
    /// 最近一次过滤词（输入框文本镜像，供订阅回调无 `&App` 时快速重算）。
    query: String,
    /// 过滤后可见条目在 `items` 内的下标（高分在前、稳定有序）。
    filtered: Vec<usize>,
    /// 选中行的（`filtered` 内）下标。
    selected: Option<usize>,
    /// 打开面板前的焦点句柄（关闭时归还）。
    previous_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
    /// 列表滚动句柄：`#command-palette-scroll`（`overflow_y_scroll` + `track_scroll`）
    /// 跟踪它，右侧 `Scrollbar::vertical` 自绘跟随（宿主原生滚动条在此 GPUI 不绘制）。
    scroll_handle: ScrollHandle,
}

impl CommandPalette {
    /// 宿主创建：模式 + 候选条目 + 打开前的焦点句柄。创建即挂好输入框与订阅。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        mode: PaletteMode,
        items: Vec<PaletteItem>,
        previous_focus: Option<FocusHandle>,
    ) -> Self {
        let placeholder = match mode {
            PaletteMode::Commands => "输入命令名以过滤…",
            PaletteMode::Files => "输入文件名以过滤已打开文件…",
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let input_entity = input.clone();
        let _input_sub = cx.subscribe(&input, move |this, _e, ev: &InputEvent, cx| match ev {
            InputEvent::Change => {
                this.query = input_entity.read(cx).value().to_string();
                this.recompute_filter();
                this.selected = (!this.filtered.is_empty()).then_some(0);
                cx.notify();
            }
        });
        let count = items.len();
        Self {
            mode,
            input,
            _input_sub,
            query: String::new(),
            filtered: (0..count).collect(),
            selected: (!items.is_empty()).then_some(0),
            items,
            previous_focus,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    /// 重算过滤：对全部条目打分、稳定降序，得出可见序。
    fn recompute_filter(&mut self) {
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| fuzzy_score(&self.query, &it.label).map(|s| (s, i)))
            .collect();
        // `sort_by_key` 与 `sort_by` 同为稳定排序：同分保持原顺序。
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
    }

    /// 选中行移动（±1，环绕）；过滤为空则无选中。
    fn select_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.filtered.len();
        if n == 0 {
            self.selected = None;
            return;
        }
        self.selected = Some(match self.selected {
            Some(ix) => (ix as isize + delta).rem_euclid(n as isize) as usize,
            None if delta > 0 => 0,
            None => n - 1,
        });
        cx.notify();
    }

    /// 执行当前选中的条目并关闭面板。
    ///
    /// 处理器若已把焦点带离面板（打开文件/新建聚焦新编辑区），不再归还旧焦点；
    /// 否则（命令未动焦点，如跟随系统主题）把焦点还给打开面板前的控件。
    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sel) = self.selected else {
            return;
        };
        let Some(&idx) = self.filtered.get(sel) else {
            return;
        };
        let runner = self.items[idx].run.clone();
        cx.stop_propagation();
        runner(window, cx);
        let focus_left = self.focus_left(window, cx);
        self.selected = None;
        cx.emit(DismissEvent);
        if !focus_left
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    /// Esc / 点击面板外：关闭并把焦点还给此前控件。宿主 toggle 关闭也走此路径。
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus_left = self.focus_left(window, cx);
        self.selected = None;
        cx.stop_propagation();
        window.prevent_default();
        cx.emit(DismissEvent);
        if !focus_left
            && let Some(handle) = self.previous_focus.take()
        {
            window.focus(&handle, cx);
        }
    }

    /// 焦点是否已离开面板（面板根或查询输入框之外）。
    fn focus_left(&self, window: &Window, cx: &App) -> bool {
        let Some(f) = window.focused(cx) else {
            return false;
        };
        !(f == self.focus_handle || f == self.input.read(cx).focus_handle(cx))
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_ref() {
            "up" => {
                cx.stop_propagation();
                self.select_step(-1, cx);
            }
            "down" => {
                cx.stop_propagation();
                self.select_step(1, cx);
            }
            "enter" => {
                if self.selected.is_some() {
                    cx.stop_propagation();
                    window.prevent_default();
                    self.activate(window, cx);
                }
            }
            "escape" => {
                cx.stop_propagation();
                self.dismiss(window, cx);
            }
            _ => {}
        }
    }

    fn on_mouse_down_out(
        &mut self,
        _e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss(window, cx);
    }

    /// 查询输入框的焦点句柄（宿主打开时聚焦它）。
    pub(crate) fn input_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// 查询输入框实体（测试驱动过滤用：`set_value` 发 `Change` → 本面板重算）。
    #[allow(dead_code)] // 测试驱动入口（app::tests 用）
    pub(crate) fn input_entity(&self) -> Entity<InputState> {
        self.input.clone()
    }

    // -- 测试可见性 --

    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn mode(&self) -> PaletteMode {
        self.mode
    }

    /// 列表滚动句柄（app::tests 断言「列表溢出可在容器内滚动」用，读 `max_offset()`）。
    #[allow(dead_code)] // 测试驱动入口（app::tests 用）
    pub(crate) fn scroll_handle(&self) -> ScrollHandle {
        self.scroll_handle.clone()
    }

    /// 当前查询词。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// 可见条目数（过滤后）。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn visible_len(&self) -> usize {
        self.filtered.len()
    }

    /// 选中行的（可见列表内）下标。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// 可见行 `row` 的主文案（测试断言过滤结果用）。
    #[allow(dead_code)] // 测试断言用（app::tests）
    pub(crate) fn visible_label(&self, row: usize) -> Option<&str> {
        self.filtered
            .get(row)
            .and_then(|&i| self.items.get(i))
            .map(|it| it.label.as_str())
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}
impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let (colors, is_dark) = (theme.colors, theme.is_dark());
        let surface: Hsla = colors.surface.into();
        let fg: Hsla = colors.fg.into();
        let fg_muted: Hsla = colors.fg_muted.into();
        let chrome = ChromeColors::for_mode(is_dark);
        let selected_bg: Hsla = chrome.menu_highlight.into();

        // 面板高度 = 窗口 2/3：列表项多时不再撑出屏幕底部（超出部分列表区滚动）。
        let panel_h = window.bounds().size.height.as_f32() * (2.0 / 3.0);
        // 列表滚动句柄（跨帧稳定）：`#command-palette-scroll` 跟踪它，右侧
        // `Scrollbar::vertical` 自绘跟随（宿主在此 GPUI 不绘制可见滚动条）。
        let scroll_handle = self.scroll_handle.clone();

        // 顶部查询框：聚焦时边框点亮为主题 focus_ring（与侧栏搜索框同款 Fluent 视觉）。
        let focused = window.focused(cx) == Some(self.input.read(cx).focus_handle(cx));
        let input_border: Hsla = if focused {
            colors.focus_ring.into()
        } else {
            colors.border.into()
        };
        let hint = match self.mode {
            PaletteMode::Commands => "命令面板",
            PaletteMode::Files => "打开文件",
        };
        let query_box = div()
            .id("command-palette-query")
            .debug_selector(|| "command-palette-query".into())
            .px_3()
            .py_2()
            .mx_3()
            .mt_2()
            .mb_1()
            .rounded_md()
            .border_1()
            .border_color(input_border)
            .child(Input::new(&self.input));

        // 列表行。
        let rows: Vec<AnyElement> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(row_ix, &item_ix)| {
                let item = &self.items[item_ix];
                let is_selected = self.selected == Some(row_ix);
                let row = div()
                    .id(ElementId::from(format!("command-palette-item-{row_ix}")))
                    .debug_selector(move || format!("command-palette-item-{row_ix}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .min_h(px(ITEM_HEIGHT))
                    .mx_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(fg)
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.selected = Some(row_ix);
                        } else if this.selected == Some(row_ix) {
                            this.selected = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = Some(row_ix);
                        this.activate(window, cx);
                    }))
                    // P2-10：选中/悬停高亮淡入（键盘 ↑↓ 与鼠标同一弹簧曲线）。
                    .with_spring(
                        format!("command-palette-item-bg-{row_ix}"),
                        hover_spring(if is_selected { 1.0 } else { 0.0 }),
                        move |d, v| d.bg(selected_bg.opacity(v)),
                    );
                row.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(13.))
                        .child(item.label.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(fg_muted)
                                .child(item.detail.clone()),
                        )
                        .when(!item.shortcut.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(chrome.text_tertiary)
                                    .child(item.shortcut.clone()),
                            )
                        }),
                )
                .into_any_element()
            })
            .collect();

        // 空态提示（命令模式「无匹配命令」；文件模式按有无文件区分文案）。
        let list_empty: Option<AnyElement> = if rows.is_empty() {
            let msg = match self.mode {
                PaletteMode::Commands => "无匹配命令",
                PaletteMode::Files => {
                    if self.items.is_empty() {
                        "尚未打开任何文件"
                    } else {
                        "无匹配文件"
                    }
                }
            };
            Some(
                div()
                    .px_3()
                    .py_3()
                    .text_size(px(12.))
                    .text_color(fg_muted)
                    .child(div().text_center().child(msg))
                    .into_any_element(),
            )
        } else {
            None
        };

        let panel = div()
            .id("command-palette")
            .debug_selector(|| "command-palette".into())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down_out(cx.listener(Self::on_mouse_down_out))
            .flex()
            .flex_col()
            .w(px(600.))
            .h(px(panel_h))
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(surface.opacity(if is_dark {
                FLOAT_ACRYLIC_ALPHA_DARK
            } else {
                FLOAT_ACRYLIC_ALPHA_LIGHT
            }))
            // 与菜单/通知同款双层阴影（Win11 浮动卡片）。
            .shadow(vec![
                gpui::BoxShadow::new(px(0.), px(8.), gpui::hsla(0., 0., 0., 0.20))
                    .blur_radius(px(14.)),
                gpui::BoxShadow::new(px(0.), px(2.), gpui::hsla(0., 0., 0., 0.14))
                    .blur_radius(px(4.)),
            ])
            // 打开淡入（尊重 reduce_motion：`with_spring` 自动落端点不调度帧）。
            .with_spring(
                "command-palette-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(11.)).text_color(fg_muted).child(hint)),
            )
            .child(query_box)
            .child(
                div()
                    .id("command-palette-list")
                    .debug_selector(|| "command-palette-list".into())
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("command-palette-scroll")
                            .debug_selector(|| "command-palette-scroll".into())
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .p(px(4.))
                            .child(div().flex().flex_col().gap_y(px(1.)).children(rows))
                            .when_some(list_empty, |d, e| d.child(e)),
                    )
                    .child(
                        div()
                            .id("command-palette-vscrollbar")
                            .debug_selector(|| "command-palette-vscrollbar".into())
                            .absolute()
                            .inset_0()
                            .child(
                                Scrollbar::vertical(&scroll_handle)
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            );

        // 合成层纪律（同通知/菜单）：整层 deferred 独立绘制 + 全窗包裹做水平居中，
        // 面板自身挂 on_mouse_down_out（点击面板外关闭，机制与 PopupMenu 一致）。
        //
        // 命中测试警示：
        // ① 全窗包裹必须是 absolute 的**非 flex** 容器。本 GPUI 版本下
        //   `absolute + flex(items_start/justify_center)` 作为 deferred 直子时，其子元素
        //   （面板/查询输入框/列表行）注册的 hitbox 位置失真，点击全穿透到下层编辑器
        //   （2026-10-09 最小结构复现：v0 失败、显式 left/top 定位成功）。
        // ② 全窗包裹要 `occlude()`（BlockMouse）——否则点击虽会命中面板元素，但仍会
        //   继续命中其下方编辑器的 hitbox，编辑器 on_mouse_down 随后把焦点抢回去
        //   （输入框 `on_mouse_down` 不 stop_propagation）。occlude 让 hit-test 停在
        //   本层，点击不再穿透到编辑器。
        let win_w = window.bounds().size.width.as_f32();
        let x = ((win_w - 600.) / 2.).max(8.);
        gpui::deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .child(div().absolute().left(px(x)).top(px(96.)).child(panel)),
        )
        .with_priority(210)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模糊得分纯函数：前缀 > 子串 > 乱序子序列；大小写不敏感；不匹配剔除。
    #[test]
    fn fuzzy_score_rules() {
        // 前缀命中高于子串命中
        assert!(
            fuzzy_score("文件", "文件保存").unwrap() > fuzzy_score("文件", "保存文件").unwrap()
        );
        // 子串命中的位置越靠前得分越高
        assert!(
            fuzzy_score("保存", "保存文件").unwrap()
                > fuzzy_score("保存", "文件保存").unwrap()
        );
        // 子串命中高于乱序子序列（"打件" 在 "打开文件…" 中是跨间隔的子序列）
        assert!(
            fuzzy_score("文件", "打开文件…").unwrap() > fuzzy_score("打件", "打开文件…").unwrap()
        );
        assert!(fuzzy_score("打件", "打开文件…").is_some(), "有效子序列应命中");
        // 大小写不敏感（前缀 + 子串均不敏感）
        assert!(fuzzy_score("ctrl", "Ctrl+S").is_some());
        assert!(fuzzy_score("CTRL", "Ctrl+S Ctrl+O").is_some());
        // 不匹配 → None（子序列要求按标签字符顺序出现）
        assert_eq!(fuzzy_score("xyzabc", "打开文件"), None);
        assert_eq!(fuzzy_score("件打文开", "打开文件…"), None, "逆序字符不进子序列");
        // 空查询匹配全部（得分 0，保持原顺序）
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        let _ = fuzzy_score("", "");
    }
}
