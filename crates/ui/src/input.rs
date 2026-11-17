//! 单行文本输入框（#12）：替代 gpui_component 的搜索框 `Input`/`InputState`/`InputEvent`。
//!
//! 自研实现：`InputState`（实体：文本/占位符/焦点/光标）+ `EntityInputHandler`
//! （IME/文本输入）+ 自定义 [`InputElement`]（paint 期挂 `window.handle_input`、画文本/
//! 占位符/光标）。键盘（Backspace/Delete/方向/Home/End/Ctrl 组合）走本地 `crate::actions`
//! 的 "Input" keymap（#10 自绑）：元素 `key_context("Input")` + `on_action` 接线；
//! Undo/Redo 已实现（内联栈，`UNDO_LIMIT` 深度 + 内嵌合并）。
//!
//! IME 状态机本体已收敛到 `ime`（M2.5 双份合一）：本文件只实现
//! [`ImeTextHost`] 适配层（byte 语义、`&str` 遍历的 UTF-16 换算、经 `edit_range`
//! 落地 + 发 `Change`）。**维护 marked 区间**：Windows 组合更新以
//! `replace_and_mark_text_in_range(None, …)` 下发（None = 替换当前 marked/选区），
//! 不跟踪 marked 会把每键组合**追加**到光标后（中文 IME 乱码「拼音历史拼接」）——
//! 跟踪后组合更新正确替换旧组合、提交整体落地。单行框仍不画组合下划线（Windows IME
//! 自带组合窗口），组合内偏移只摆光标、不建选区。

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    div, point, size, AnyElement, App, Bounds, ClipboardItem, Context, Element, ElementId, Entity,
    EntityInputHandler, EventEmitter, Focusable, FocusHandle, GlobalElementId, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Styled as _,
    TextInputConfiguration, TextRun, UTF16Selection, Window, px,
};

use crate::actions::{
    Backspace, Copy, Cut, Delete, DeleteToNextWordEnd, DeleteToPreviousWordStart, MoveEnd,
    MoveHome, MoveLeft, MoveRight, MoveToEnd, MoveToNextWord, MoveToPreviousWord, MoveToStart,
    PasteNormalized, Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd,
    SelectToPreviousWordStart, SelectToStart, SelectToStartOfLine, Undo,
};
use config::layout::DEFAULT_FONT_SIZE;
// P3a 断环关键点：EditIntent 已下沉 base，本 crate 不再经 editor 引用（editor↔ui 环断开）。
use base::EditIntent;
use ime::{self, ImeTextHost};
use crate::ui::theme::Theme;

/// 单行输入框事件（仅实现 App 用到的最小面）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// 文本内容变化（搜索框变化 → 刷新搜索结果）。
    Change,
    /// Enter 按下（Shift+Enter 时 `shift=true`）；组件已 `stop_propagation` +
    /// `prevent_default`。消费方订阅即可复用「提交」语义，无需在容器层手工解析按键。
    Submit { shift: bool },
    /// Esc 按下；组件已 `stop_propagation`。消费方订阅复用「取消/关闭」语义。
    Cancel,
}

/// 左右内边距（文本/光标相对元素左缘）。
const PAD_PX: f32 = 2.0;
/// 光标宽。
const CARET_PX: f32 = 1.0;
/// 光标闪烁周期（ms，对齐编辑器 `BLINK_MS`）。
const CARET_BLINK_MS: u64 = 500;
/// 光标交互后的静止期（ms，期间光标常亮不闪，对齐编辑器 `BLINK_PAUSE_MS`）。
const CARET_BLINK_PAUSE_MS: u64 = 300;
/// 撤销/重做栈深度上限（超出丢最旧；对齐 wysiwyg `UNDO_LIMIT`=200）。
const UNDO_LIMIT: usize = 200;
/// 内嵌编辑合并时间窗（ms）：两次连续内嵌编辑间隔 ≥ 此值即新起撤销单元
/// （对齐 Zed `History::group_interval` / editor_core `UNDO_GROUP_MS` = 300ms）。
const UNDO_GROUP_MS: u64 = 300;
/// 显式 `text_color` 注入时占位符的低不透明度系数（在其上派生「次要文字」观感）。
const PLACEHOLDER_ALPHA: f32 = 0.5;

// -------------------------------------------------------------------------
// P3/D1 声明式参数面：尺寸档 / 粘贴模式 / 提交校验（全部默认 = 现状行为，
// 既有 8 个消费方不传参数即原样）。
// -------------------------------------------------------------------------

/// 输入框尺寸档。
///
/// 尺寸骨架（像素）统一走 `InputSize` 命名方法，禁在 render/paint 裸写魔数。
/// 各档映射（高度/字号/行高 三轨等比缩放；左右内边距暂统一 `PAD_PX`，Regular 不变）：
/// - 高度（px）：Compact 20 / Regular 24（现状 h24）/ Large 28；
/// - 正文字号（px）：Compact 13 / Regular 14（=`DEFAULT_FONT_SIZE` 现状）/ Large 16；
/// - 行高（px）：字号 × 行高系数，系数 Compact 1.2 / Regular 1.4（现状）/ Large 1.6。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputSize {
    /// 现状外观（h24 / 字14 / 行高 1.4×14 / pad2）。
    #[default]
    Regular,
    /// 紧凑档（h20）。
    Compact,
    /// 大档（h28）。
    Large,
}

impl InputSize {
    /// 框高（px）。
    fn height_px(&self) -> f32 {
        match self {
            InputSize::Compact => 20.0,
            InputSize::Regular => 24.0,
            InputSize::Large => 28.0,
        }
    }

    /// 正文字号（px），Regular = 现状 `DEFAULT_FONT_SIZE`。
    fn font_size_px(&self) -> f32 {
        match self {
            InputSize::Compact => DEFAULT_FONT_SIZE - 1.0,
            InputSize::Regular => DEFAULT_FONT_SIZE,
            InputSize::Large => DEFAULT_FONT_SIZE + 2.0,
        }
    }

    /// 行高系数（行高 = 字号 × 系数），Regular = 现状 1.4。
    fn line_height_factor(&self) -> f32 {
        match self {
            InputSize::Compact => 1.2,
            InputSize::Regular => 1.4,
            InputSize::Large => 1.6,
        }
    }

    /// 行高（px）。
    fn line_height(&self) -> Pixels {
        px(self.font_size_px() * self.line_height_factor())
    }
}

/// 粘贴模式：对剪贴板文本换行的处理策略（默认 Passthrough = 现状）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PasteMode {
    /// 现状：`\r\n`/`\r` → `\n` 后进入编辑管线；最终由 `edit_range` 的单行不变式
    /// 把换行折叠为空格（单行框不保存 `\n`，防 `shape_line` panic）。
    #[default]
    Passthrough,
    /// 折叠：插入文本中每个换行折叠为单个空格（"a\r\nb\nc" → "a b c"）。
    StripNewlines,
    /// 拒绝：含换行的粘贴整段不插入（文本框保持原样）。
    RejectMultiline,
}

/// 提交校验结果：`Ok` = 放行 Enter 提交；`Err(Option<String>)` = 拦截提交，
/// 错误消息先存不用（预留后续展示），本批不落 UI。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Validation {
    Ok,
    Err(Option<String>),
}

/// 写入分类（撤销记录粒度）：`edit_range` 是组件唯一文本写入口，所有文本变更按此登记
/// 撤销历史（R4 单点登记，防漏记/重记）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoClass {
    /// IME 组合更新（组合键按下中）：不产生撤销单元；组合首键锚定「组合起点快照」，
    /// 组合提交时整体作为一组入栈。
    ImeCompose,
    /// 内嵌直输/退格/删除/词删除：与同一段连续内嵌编辑（<300ms 且无光标移动）合并为
    /// 一个撤销单元。
    Inline,
    /// 独立一个撤销单元：粘贴 / IME 组合提交 / 剪切（不合并）。
    Atomic,
}

/// 进行中的内嵌撤销合并单元（仿 wysiwyg `open_unit` / editor_core `pending`）：
/// 记录组起始的全文+选区快照与上次编辑时刻，供 300ms 时间窗合并判定。
/// 合并只延伸「组终点」，`text_before`/`sel_before` 保持组起始态（= 撤销目标）。
struct UndoOpen {
    text_before: String,
    sel_before: Range<usize>,
    last_edit_at: Instant,
}

/// 单行输入框状态（实体）：文本 + 占位符 + 焦点 + 光标（字节区间）+ 元素边界。
pub struct InputState {
    text: String,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    selection: Range<usize>,
    /// 选区活动端点（head）：光标所在端（`selection` 恒为 min..max）。
    /// 拖选/Shift 移动时 head 随鼠标/按键走、锚点（另一端）不动；光标绘制看 head。
    selection_head: usize,
    /// 鼠标拖选中（元素内已按下、尚未松开）。`on_mouse_up`/`on_mouse_up_out`/失焦复位。
    dragging: bool,
    /// 光标可见性（闪烁驱动）。
    caret_visible: bool,
    /// 光标交互后静止期起点（期间光标常亮不闪）。
    caret_pause_until: Option<Instant>,
    /// 闪烁代际：失焦/重新聚焦后旧循环自然退出。
    blink_epoch: u64,
    blink_running: bool,
    /// 本实体是否持焦（on_focus/on_blur + render 对齐维护，供闪烁循环判退）。
    focused: bool,
    /// 焦点事件是否已注册（仅一次）。
    focus_registered: bool,
    /// 组合中（marked）区间（字节）。Windows 组合更新 `range=None` = 替换它；
    /// 提交/取消后由共享核心清空。单行框不画组合下划线，但必须跟踪以保证「替换不追加」。
    marked: Option<Range<usize>>,
    element_bounds: Bounds<Pixels>,
    /// 水平滚动偏移（px，≥0）；0 = 文本从框左缘显示，实际绘制时文本整体左移 `scroll_x`。
    /// 在 paint 期按光标须落在可见带内重算并写回（避免把 `&mut Window` 穿进每个移动
    /// 光标的方法）；失焦复位 0（显示头部）。
    scroll_x: f32,
    /// 尺寸档（框高/字号/行高骨架）。
    size: InputSize,
    /// 粘贴模式（换行处理策略）。
    paste_mode: PasteMode,
    /// 提交校验器（Enter 提交前，配合 `confirm_required` 用）；`None` = 不校验。
    validate: Option<fn(&str, &Window) -> Validation>,
    /// 提交需确认：`true` 时仅校验通过才 emit `Submit`；默认 `false` = 现状。
    confirm_required: bool,
    /// 显式文字色（None = 沿用 `Theme::global` 的 fg/fg_muted）。供消费方在**固定深色面**
    /// （如 music 深紫 pill）注入不随全局深浅翻转的文字色；占位符在其低不透明度派生。
    text_color: Option<Hsla>,
    /// 撤销栈：每个条目 = 该组起始的全文 + 字节选区快照（撤销目标）；深度上限
    /// [`UNDO_LIMIT`]，超出丢最旧。各 InputState 实体各自独立。
    undo_stack: Vec<(String, Range<usize>)>,
    /// 重做栈：条目 = 撤销瞬间被回退的当前态（重做目标）；新编辑后整棵作废（历史分叉）。
    redo_stack: Vec<(String, Range<usize>)>,
    /// 进行中的内嵌合并单元（None = 无）；Enter 提交/粘贴/撤销等动作封口入栈。
    undo_open: Option<UndoOpen>,
    /// 是否允许后续内嵌编辑并入当前组：**纯光标移动（方向/点按/拖选/全选）置 false 断层**
    /// （对齐 Zed `set_cooperative`）；文本编辑落地置 true。
    cooperative: bool,
    /// IME 组合起点快照（组合首键锚定；组合提交时作撤销单元「编辑前」态；非组合编辑清掉）。
    composition_anchor: Option<(String, Range<usize>)>,
}

impl InputState {
    /// 创建输入框（原 gpui_component 签名，`window` 不再需要）。
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            placeholder: SharedString::default(),
            focus_handle: cx.focus_handle(),
            selection: 0..0,
            selection_head: 0,
            dragging: false,
            caret_visible: false,
            caret_pause_until: None,
            blink_epoch: 0,
            blink_running: false,
            focused: false,
            focus_registered: false,
            marked: None,
            element_bounds: Bounds::default(),
            scroll_x: 0.0,
            size: InputSize::default(),
            paste_mode: PasteMode::default(),
            validate: None,
            confirm_required: false,
            text_color: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_open: None,
            cooperative: true,
            composition_anchor: None,
        }
    }

    /// 占位符（为空文本且未聚焦时显示）。
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// 初始文本。
    /// 单行不变式：换行折叠为空格（同 `edit_range`，防 `shape_line` panic）。
    pub fn default_value(mut self, value: impl Into<String>) -> Self {
        self.text = value.into().replace('\n', " ");
        let len = self.text.len();
        self.selection = len..len;
        self.selection_head = len;
        self
    }

    /// 尺寸档（默认 Regular = 现状外观；既有消费方不传即原样）。
    pub fn size(mut self, size: InputSize) -> Self {
        self.size = size;
        self
    }

    /// 粘贴模式（默认 Passthrough = 现状：`\r\n`→`\n` 原样插入）。
    pub fn paste_mode(mut self, mode: PasteMode) -> Self {
        self.paste_mode = mode;
        self
    }

    /// 提交校验器（配合 `confirm_required`）：`Ok` = 放行 Enter 提交；
    /// `Err` = 拦截（消息先存不用）。默认 `None` = 不校验。
    ///
    /// ⚠️ 签名说明：brief 写作 `fn(&str, &WindowContext)`，本仓 gpui 并无
    /// `WindowContext` 类型（`Context<T>` deref 到 `App`、窗口句柄为 `Window`），
    /// 取最贴近语义的 `&Window`。
    pub fn validate(mut self, validate: fn(&str, &Window) -> Validation) -> Self {
        self.validate = Some(validate);
        self
    }

    /// 提交需确认：`true` 时 Enter 仅在 validate 通过才 emit `Submit`；
    /// 默认 `false` = 现状（Enter 恒 emit Submit）。
    pub fn confirm_required(mut self, confirm_required: bool) -> Self {
        self.confirm_required = confirm_required;
        self
    }

    /// 显式文字色（None = 沿用主题 fg/fg_muted；占位符按其低不透明度派生）。
    /// 用于固定深色面上的输入框（如 music 深紫 pill）——文字不随全局深浅翻转。
    pub fn text_color(mut self, color: Hsla) -> Self {
        self.text_color = Some(color);
        self
    }

    /// 当前文本。
    pub fn value(&self) -> SharedString {
        SharedString::from(self.text.clone())
    }

    /// 整体替换文本（程序化设置，发 `Change`）。
    /// 测试/程序化调用专用（与 gpui_component `InputState::set_value` 同 API）。
    /// 单行不变式：换行折叠为空格（同 `edit_range`，防 `shape_line` panic）。
    #[allow(dead_code)]
    pub fn set_value(
        &mut self,
        value: impl Into<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text = value.into().replace('\n', " ");
        let len = self.text.len();
        self.selection = len..len;
        self.selection_head = len;
        self.marked = None;
        // 整体替换 = 整框内容换锚：清空撤销/重做历史（旧堆栈相对新内容无意义）。
        self.clear_undo_history();
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// 整体替换文本（粘贴到搜索框路径，发 `Change`）。
    /// 单行不变式：换行折叠为空格（同 `edit_range`，防 `shape_line` panic）。
    pub fn replace(&mut self, text: impl Into<String>, _window: &mut Window, cx: &mut Context<Self>) {
        self.text = text.into().replace('\n', " ");
        let len = self.text.len();
        self.selection = len..len;
        self.selection_head = len;
        self.marked = None;
        // 整体替换 = 整框内容换锚：清空撤销/重做历史。
        self.clear_undo_history();
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn focused(&self, window: &Window, cx: &App) -> bool {
        window.focused(cx) == Some(self.focus_handle.clone())
    }

    /// 在指定字节区间替换文本并收敛光标（IME/编辑共用），发 `Change`。
    /// 任何一次落地都清掉组合态（提交/删除/粘贴都结束组合；组合更新经
    /// [`ime::replace`] 在 apply 之后重新写回 marked，不受影响）。
    ///
    /// 唯一文本写入口：所有文本变更（字符/退格/删除/粘贴/剪切/IME）都经此登记
    /// 撤销历史（R4）。`class` = 撤销分类（见 [`UndoClass`]）。
    ///
    /// 单行不变式：本框渲染走 `shape_line`（gpui 对该路径断言文本不含 `\n`，
    /// `vendor/.../text_system.rs:645`），故**任何**写入入口（粘贴/IME/程序化替换）
    /// 都把换行折叠为空格再落盘——否则多行粘贴/组合会触发 `shape_line` panic。
    /// `\n` 与 `' '` 同为 1 字节，折叠不改字节长度，光标/选区算术不受影响。
    fn edit_range(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        class: UndoClass,
        cx: &mut Context<Self>,
    ) {
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        let folded = new_text.replace('\n', " ");
        // 无实际变更（边界空删 / 空粘贴 / 空组合替换）→ 全链路 no-op：不登记撤销、不发
        // Change——空剪贴板/空文本粘贴与删到无可删都不应产生副作用（R6）。
        if start == end && folded.is_empty() {
            return;
        }
        match class {
            // 组合更新：不产生撤销单元；组合首键（尚无 marked）锚定组合起点快照，
            // 组合提交时（Atomic）整体作一组（见 record_undo）。
            UndoClass::ImeCompose => {
                if self.marked.is_none() {
                    self.composition_anchor = Some((self.text.clone(), self.selection.clone()));
                }
            }
            UndoClass::Inline | UndoClass::Atomic => self.record_undo(class),
        }
        self.text.replace_range(start..end, &folded);
        let caret = start + folded.len();
        self.selection = caret..caret;
        self.selection_head = caret;
        self.marked = None;
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    // ---------------------------------------------------------------------
    // 撤销/重做（内联轻量栈）
    //
    // 撤销历史只记组件内文本状态（值 + 选区快照），各 InputState 实体各自独立。
    // 语义与 editor_core::undo::UndoManager / Zed text.rs `History.group` 同构
    // （事务=一组编辑、300ms 时间窗合并、redo 分叉作废、上限丢最旧），但按
    // 「全文快照」存储（单行框文本短、编辑密度低，快照成本可忽略）。
    //
    // ⚠️ 不直接复用 editor_core 的 UndoManager：editor_core 依赖 ui
    // （crates/editor_core/Cargo.toml 的 `ui = { path = "../ui" }`），ui 反向依赖会
    // 成环；故在 ui 内内联同构轻量栈（对齐 `markdown_wysiwyg` types.rs:207 的
    // `Vec<(String, Range)> + UNDO_LIMIT` 模式）。若 editor_core 未来解除对 ui 的
    // 依赖，可改为直接复用 UndoManager。
    //
    // 合并边界（对齐 Zed）：
    // - 连续内嵌输入（字符/退格/删除/词删除）：无光标移动（cooperative）且距上次编辑
    //   <UNDO_GROUP_MS → 并入当前组。
    // - 粘贴 / IME 组合提交 / 剪切：各自独立一组（永不合并）。
    // - Enter 提交（Submit）封口本单元（仿 `finalize_last_transaction`）。
    // - 新编辑（set_value/replace）清空历史：整框内容已换锚。
    // - Redo 分叉作废：撤销后新编辑清空 redo 栈。
    // ---------------------------------------------------------------------

    /// 封口进行中的内嵌合并组入撤销栈（组起始态 = 撤销目标）。
    fn flush_undo_open(&mut self) {
        if let Some(open) = self.undo_open.take() {
            self.push_undo((open.text_before, open.sel_before));
        }
    }

    /// 入撤销栈（深度上限 [`UNDO_LIMIT`]，超出丢最旧）。
    fn push_undo(&mut self, state: (String, Range<usize>)) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(state);
    }

    /// 清空撤销/重做历史（整体替换/程序化写：整框内容已换锚，旧堆栈无意义）。
    fn clear_undo_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_open = None;
        self.composition_anchor = None;
        self.cooperative = true;
    }

    /// **单点撤销登记**（文本变更落地前调用，此时 `self.text`/`self.selection` 仍是
    /// 编辑前状态）：所有文本变更经 `edit_range` 汇到这里，防漏记/重记。
    ///
    /// - `Atomic`（粘贴/IME 提交/剪切）：永不合并——封口当前进行中内嵌组，本编辑自身
    ///   作为一组（before = 编辑前状态；IME 组合提交用组合起点快照）。
    /// - `Inline`（直输/退格/删除/词删除）：无光标移动且距上次内嵌编辑 <[`UNDO_GROUP_MS`]
    ///   则并入开放组（合并只延伸 after，不动 before）；否则封口旧组、开新组。
    fn record_undo(&mut self, class: UndoClass) {
        // 任何真实编辑都作废重做分支：撤销后新编辑 = 历史分叉，防 Ctrl+Y 以旧态重放。
        self.redo_stack.clear();
        let anchor = self.composition_anchor.take();
        if class == UndoClass::Atomic {
            self.flush_undo_open();
            let before = anchor.unwrap_or_else(|| (self.text.clone(), self.selection.clone()));
            self.push_undo(before);
            self.cooperative = true;
            return;
        }
        let merge = self.cooperative
            && self.undo_open.as_ref().is_some_and(|o| {
                o.last_edit_at.elapsed().as_millis() < UNDO_GROUP_MS as u128
            });
        if merge {
            if let Some(o) = self.undo_open.as_mut() {
                o.last_edit_at = Instant::now();
            }
            self.cooperative = true;
            return;
        }
        self.flush_undo_open();
        self.undo_open = Some(UndoOpen {
            text_before: self.text.clone(),
            sel_before: self.selection.clone(),
            last_edit_at: Instant::now(),
        });
        self.cooperative = true;
    }

    /// 恢复某撤销/重做目标态（全文 + 字节选区），发 Change（撤销/重做改变文本内容）。
    fn restore_state(&mut self, text: String, sel: Range<usize>, cx: &mut Context<Self>) {
        self.text = text;
        let len = self.text.len();
        let start = sel.start.min(len);
        let end = sel.end.min(len).max(start);
        self.selection = start..end;
        self.selection_head = end;
        self.marked = None;
        self.composition_anchor = None;
        self.kick_caret();
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    fn on_undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.flush_undo_open();
        if let Some((text, sel)) = self.undo_stack.pop() {
            // 被撤销的当前态进重做栈（供 Ctrl+Y 回推）。
            self.redo_stack.push((self.text.clone(), self.selection.clone()));
            self.restore_state(text, sel, cx);
        }
        // 撤销后光标运动断层（对齐 Zed undo 后 `set_cooperative(false)`）。
        self.cooperative = false;
    }

    fn on_redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        self.flush_undo_open();
        if let Some((text, sel)) = self.redo_stack.pop() {
            self.push_undo((self.text.clone(), self.selection.clone()));
            self.restore_state(text, sel, cx);
        }
        // 重做后光标运动断层（对齐 Zed redo 后 `set_cooperative(false)`）。
        self.cooperative = false;
    }

    // -- 光标/编辑原语（字节区间，始终落在 char 边界）--

    fn prev_char(&self, index: usize) -> usize {
        self.text[..index.min(self.text.len())]
            .char_indices()
            .next_back()
            .map(|(b, _)| b)
            .unwrap_or(0)
    }

    fn next_char(&self, index: usize) -> usize {
        if index >= self.text.len() {
            return self.text.len();
        }
        index
            + self.text[index..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.text.len());
        self.selection = index..index;
        self.selection_head = index;
        // 纯光标移动 → 撤销合并断层（对齐 Zed `set_cooperative(false)`）。
        self.cooperative = false;
        self.kick_caret();
        cx.notify();
    }

    /// 扩展选区：把 head（活动端点，光标所在端）移到 `index`，锚点（另一端）不动——
    /// 拖选 / Shift（含 Shift+Ctrl）方向扩选共用。选区恒归一化为 `min..max`。
    fn extend_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let index = index.min(self.text.len());
        // 锚点 = 当前非 head 端点（反选中 head==start 时锚点在 end，否则在 start）。
        let anchor = if self.selection_head == self.selection.start {
            self.selection.end
        } else {
            self.selection.start
        };
        if index >= anchor {
            self.selection = anchor..index;
        } else {
            self.selection = index..anchor;
        }
        self.selection_head = index;
        // 纯选区变更 → 撤销合并断层（对齐 Zed `set_cooperative(false)`）。
        self.cooperative = false;
        self.kick_caret();
        cx.notify();
    }

    fn move_delta(&mut self, delta: isize, cx: &mut Context<Self>) {
        let cur = self.selection_head;
        let next = if delta < 0 {
            self.prev_char(cur)
        } else {
            self.next_char(cur)
        };
        self.move_to(next, cx);
    }

    /// 往回一个词边界（字节，纯计算，不改状态）。
    fn prev_word(&self, cur: usize) -> usize {
        let mut cur = cur.min(self.text.len());
        while cur > 0 && self.text[..cur].ends_with(char::is_whitespace) {
            cur = self.prev_char(cur);
        }
        while cur > 0 && !self.text[..cur].ends_with(char::is_whitespace) {
            cur = self.prev_char(cur);
        }
        cur
    }

    /// 往前一个词边界（字节，纯计算，不改状态）。
    fn next_word(&self, cur: usize) -> usize {
        let mut cur = cur.min(self.text.len());
        while cur < self.text.len() && self.text[cur..].starts_with(char::is_whitespace) {
            cur = self.next_char(cur);
        }
        while cur < self.text.len() && !self.text[cur..].starts_with(char::is_whitespace) {
            cur = self.next_char(cur);
        }
        cur
    }

    fn move_word(&mut self, forward: bool, cx: &mut Context<Self>) {
        let head = self.selection_head;
        let next = if forward {
            self.next_word(head)
        } else {
            self.prev_word(head)
        };
        self.move_to(next, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let len = self.text.len();
        self.selection = 0..len;
        self.selection_head = len;
        // 纯选区变更 → 撤销合并断层。
        self.cooperative = false;
        self.kick_caret();
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if self.selection.start < self.selection.end
            && let Some(text) = self.text.get(self.selection.clone())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    /// 剪贴板文本 → 按 paste_mode 折叠/拒绝后的可插入串（None = 整段拒绝，不修改）。
    ///
    /// 与 Zed「保留 `\n` 显示 ⋯」的差异：我方单行框渲染走 `shape_line`（gpui 对
    /// 该路径断言文本不含 `\n`），任何写入都把换行折叠为空格（`Passthrough` 也折叠），
    /// 防单行崩溃——`PasteMode` 只在「折叠成空格」与「整段拒绝」之间选择，做不到保留
    /// 换行显示。
    fn fold_paste(&self, raw: &str) -> Option<String> {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        match self.paste_mode {
            PasteMode::Passthrough => Some(normalized),
            PasteMode::StripNewlines => Some(normalized.replace('\n', " ")),
            PasteMode::RejectMultiline => {
                if normalized.contains('\n') {
                    None
                } else {
                    Some(normalized)
                }
            }
        }
    }

    /// 粘贴落地：选区替换 + 一次 Atomic 撤销组。平台原生 `paste`（EntityInputHandler）
    /// 与 Ctrl+V（`PasteNormalized`）共用同一路径。
    fn do_paste(&mut self, raw: &str, cx: &mut Context<Self>) {
        if let Some(insert) = self.fold_paste(raw) {
            self.edit_range(self.selection.clone(), &insert, UndoClass::Atomic, cx);
        }
    }

    /// R1：Ctrl+V 粘贴收口（`PasteNormalized` action handler）。
    /// 空剪贴板不动作；多行处理按当前 PasteMode（`RejectMultiline` 时整段拒绝）。
    fn on_paste(&mut self, _: &PasteNormalized, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard()
            && let Some(text) = item.text()
        {
            self.do_paste(&text, cx);
        }
    }

    /// R2：Ctrl+X 剪切收口（`Cut` action handler）。
    /// 非空选区：写剪贴板 + 删选区 + 一次 Atomic 撤销组。
    /// 空选区剪切 = 无动作：不采纳编辑器「空选区复制整行」惯例（Zed `cut_common` 的
    /// 单行延伸）——单行输入无「整行」语义，切割空选区只能清空文本，与用户预期不符。
    fn on_cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let sel = self.selection.clone();
        if sel.start >= sel.end {
            return;
        }
        if let Some(text) = self.text.get(sel.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
        self.edit_range(sel, "", UndoClass::Atomic, cx);
    }

    /// 删除：`backward` 退格 / `forward` 删除；有选区则删选区。
    fn delete(&mut self, backward: bool, cx: &mut Context<Self>) {
        let sel = self.selection.clone();
        if sel.start < sel.end {
            self.edit_range(sel, "", UndoClass::Inline, cx);
        } else if backward {
            let start = self.prev_char(sel.start);
            self.edit_range(start..sel.start, "", UndoClass::Inline, cx);
        } else {
            let end = self.next_char(sel.end);
            self.edit_range(sel.end..end, "", UndoClass::Inline, cx);
        }
    }

    fn delete_word(&mut self, backward: bool, cx: &mut Context<Self>) {
        let sel = self.selection.clone();
        if sel.start < sel.end {
            self.edit_range(sel, "", UndoClass::Inline, cx);
        } else if backward {
            let start = self.prev_word(sel.start);
            self.edit_range(start..sel.start, "", UndoClass::Inline, cx);
        } else {
            let end = self.next_word(sel.end);
            self.edit_range(sel.end..end, "", UndoClass::Inline, cx);
        }
    }

    /// 点按 → 字节偏移（文本前缀宽度二分；用克隆规避对 `self` 的双重可变借用）。
    fn index_at_x_impl(&self, x: Pixels, window: &mut Window) -> usize {
        if self.text.is_empty() {
            return 0;
        }
        let run = TextRun {
            len: self.text.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(
                SharedString::from(self.text.clone()),
                px(self.size.font_size_px()),
                &[run],
                None,
            );
        // 调用方传入的已是「框内 X - PAD」；文本被左移 scroll_x 后，屏幕上该坐标对应的
        // 行内坐标 = X - PAD + scroll_x，故这里统一加回（保持「点击的字符」与「屏幕可见
        // 的字符」一致；on_mouse_down / character_index_for_point 都经本函数，无需各改）。
        let x_f = x.as_f32() + self.scroll_x;
        let text = self.text.as_str();
        let mut cursor = 0usize;
        for (i, c) in text.char_indices() {
            let next = i + c.len_utf8();
            if line.x_for_index(next).as_f32() <= x_f {
                cursor = next;
            } else {
                break;
            }
        }
        cursor
    }

    /// 前缀文本整形宽度（px，空串 → 0）。供光标 x、拖选两端 x 共用。
    fn text_width(&self, s: &str, window: &mut Window) -> Pixels {
        if s.is_empty() {
            return px(0.);
        }
        let run = TextRun {
            len: s.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(
                SharedString::from(s.to_string()),
                px(self.size.font_size_px()),
                &[run],
                None,
            )
            .width()
    }

    /// 光标矩形 x（元素原点 + pad + 前缀宽，文本左移 scroll_x），供 IME `bounds_for_range`。
    fn caret_px(&self, byte: usize, element_bounds: Bounds<Pixels>, window: &mut Window) -> Pixels {
        let prefix = &self.text[..byte.min(self.text.len())];
        element_bounds.origin.x + px(PAD_PX) - px(self.scroll_x) + self.text_width(prefix, window)
    }

    // -- 事件回调 --

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(true, cx);
    }

    fn on_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(false, cx);
    }

    fn on_delete_prev_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(true, cx);
    }

    fn on_delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_word(false, cx);
    }

    fn on_move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_delta(-1, cx);
    }

    fn on_move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_delta(1, cx);
    }

    fn on_move_home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn on_move_end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn on_move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn on_move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn on_move_prev_word(
        &mut self,
        _: &MoveToPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_word(false, cx);
    }

    fn on_move_next_word(
        &mut self,
        _: &MoveToNextWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_word(true, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy(cx);
    }

    fn on_select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(0, cx);
    }

    fn on_select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.text.len(), cx);
    }

    fn on_select_to_start_of_line(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to(0, cx);
    }

    fn on_select_to_end_of_line(
        &mut self,
        _: &SelectToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.extend_to(self.text.len(), cx);
    }

    fn on_select_prev_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection_head;
        self.extend_to(self.prev_word(head), cx);
    }

    fn on_select_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let head = self.selection_head;
        self.extend_to(self.next_word(head), cx);
    }

    /// 统一按键：只处理 keymap 未覆盖的 **Shift+← / Shift+→** 逐字符扩选（与主编辑器
    /// `CodeEditor::on_key_down` 同套路——`Select{Left,Right}` 是组件/编辑器内部 action、
    /// 公共 keymap 未绑我们无法注册 handler，故在原样 key_down 层处理）；其余全走
    /// `on_action`。可打印字符（无修饰）放行给 IME/输入处理。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Enter/Esc：组件统一收敛为「提交/取消」事件上抛（行为下取），**必须先于**可打印
        // 字符早退处理。
        //
        // ⚠️ 机制边界（评审实证）：gpui 按键派发次序是 **先 keymap 动作层、后 key_down
        // 事件层**（`window.rs dispatch_key_event`：`for binding in match_result.bindings {
        // dispatch_action_on_node }` → `finish_dispatch_key_event` 才派发 key_down）。
        // 因此本层 `stop_propagation` **拦不住祖先节点的 `on_action` 动作**——它只拦截
        // 祖先的 key_down 处理。
        // 本批 4 个消费方（prompt/find/goto/palette）祖先链均无 `on_action(Enter/Escape)`
        //（全仓核实），所以「key_down 层 stop_propagation 即不冒泡」语义成立。
        // 已知遗留边界：Enter/Esc 走 action 层分发（先于 key_down 层）——祖先链若注册
        // `on_action(Enter/Esc)` 会在本层 key_down stop 之前被接管（既有已知边界，
        // 机制详述见上「机制边界」）；当前实例：WYSIWYG 根挂 `on_action(Enter→newline)`
        // + CONTEXT keybinding（markdown_wysiwyg/src/wysiwyg/mod.rs:519），且 deferred
        // 浮层经 `set_active_node(prepaint_deferred_draws, window.rs:3931)` 挂回 WyEditor
        // dispatch 路径 → 图片编辑卡输入框内按 Enter 会泄漏触发文档换行：**既有 bug，
        // 本批不动，下一批专项修**。
        match event.keystroke.key.to_ascii_lowercase().as_str() {
            "enter" => {
                cx.stop_propagation();
                window.prevent_default();
                // Enter 提交封口当前撤销单元（仿 `finalize_last_transaction`）：后续
                // 编辑从提交后状态起新组，避免与提交前输入混入同一撤销单元。
                self.flush_undo_open();
                // confirm_required + validate：Err → 拦截提交（不 emit Submit），
                // 其余键行为不变。默认（confirm_required=false）= 现状恒 Submit。
                if !self.blocked_submit(window, cx) {
                    cx.emit(InputEvent::Submit {
                        shift: event.keystroke.modifiers.shift,
                    });
                }
                return;
            }
            "escape" => {
                cx.stop_propagation();
                cx.emit(InputEvent::Cancel);
                return;
            }
            _ => {}
        }
        let k = &event.keystroke;
        let m = k.modifiers;
        if !m.control && !m.alt && !m.platform && k.key_char.is_some() {
            return;
        }
        if m.shift && !m.control && !m.alt && !m.platform {
            match k.key.to_ascii_lowercase().as_str() {
                "left" => self.extend_to(self.prev_char(self.selection_head), cx),
                "right" => self.extend_to(self.next_char(self.selection_head), cx),
                _ => {}
            }
        }
    }

    /// confirm_required=true 且 validate 返回 Err → 拦截提交（Enter 不 emit Submit）；
    /// 默认（confirm_required=false 或未设 validate）= 现状：Enter 恒 Submit。
    fn blocked_submit(&self, window: &mut Window, _cx: &mut Context<Self>) -> bool {
        if !self.confirm_required {
            return false;
        }
        if let Some(validate) = self.validate {
            if let Validation::Err(_) = validate(self.text.as_str(), window) {
                return true;
            }
        }
        false
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.button != MouseButton::Left {
            return;
        }
        self.focus_handle.focus(window, cx);
        let x = (ev.position.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        let index = self.index_at_x_impl(x, window);
        // Shift+点按 = 从当前光标扩选到点击处（编辑器同语义）；否则落点摆光标。
        if ev.modifiers.shift {
            self.extend_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
        self.dragging = true;
        cx.notify();
    }

    /// 拖选：按住左键移动 → 扩展选区 head 到鼠标落点（锚点固定在按下处）。
    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let x = (ev.position.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        let index = self.index_at_x_impl(x, window);
        self.extend_to(index, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 松开点落在元素 hitbox 外 / 顶层叠层 / 窗口外：元素级 `on_mouse_up`（受
    /// `hitbox.is_hovered` 门控）不派发，本钩子（Capture 阶段）无条件收口拖选态
    /// （对齐本仓 scrollbar / editor / wysiwyg 的 `on_mouse_up_out` 拖拽清态惯例）。
    fn on_mouse_up_out(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.cancel_mouse_drag();
    }

    /// 拖选态生命周期终点之一：松开/失焦复位 `dragging`（对齐编辑器
    /// `CodeEditor::cancel_mouse_drag`；只清拖拽态，不动光标/选区）。
    fn cancel_mouse_drag(&mut self) {
        self.dragging = false;
    }

    // ---------------------------------------------------------------------
    // 光标闪烁（对齐编辑器 `CodeEditor` 的 blink 实现）
    // ---------------------------------------------------------------------

    fn on_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = true;
        self.kick_caret();
        if !self.blink_running {
            self.start_blink(cx);
        }
        cx.notify();
    }

    fn on_blur(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.focused = false;
        self.caret_visible = true;
        self.cancel_mouse_drag();
        // 失焦显示头部（再次聚焦后由 paint 期按光标重滚）。
        self.scroll_x = 0.0;
        cx.notify();
    }

    fn start_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_running {
            return;
        }
        self.blink_running = true;
        self.blink_epoch = self.blink_epoch.wrapping_add(1);
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            let elapsed = this
                .update(cx, |this, _| {
                    this.caret_pause_until
                        .map_or(u128::MAX, |p| p.elapsed().as_millis())
                })
                .unwrap_or(u128::MAX);
            if elapsed >= CARET_BLINK_PAUSE_MS as u128 {
                let _ = this.update(cx, |this, cx| {
                    this.caret_visible = !this.caret_visible;
                    cx.notify();
                });
            }
            cx.background_executor()
                .timer(Duration::from_millis(CARET_BLINK_MS))
                .await;
            let keep = this
                .update(cx, |this, _| this.focused && epoch == this.blink_epoch)
                .unwrap_or(false);
            if !keep {
                this.update(cx, |this, _| this.blink_running = false).ok();
                break;
            }
        })
        .detach();
    }

    /// 光标交互后常亮静止期（拖选/键移后先亮住再继续闪）。
    fn kick_caret(&mut self) {
        self.caret_visible = true;
        self.caret_pause_until = Some(Instant::now());
    }

    /// 当前帧光标是否应绘制：失焦不画；交互静止期内常亮；否则跟随闪烁翻转。
    fn caret_shown_now(&self) -> bool {
        if !self.focused {
            return false;
        }
        let paused = self
            .caret_pause_until
            .is_some_and(|p| p.elapsed() < Duration::from_millis(CARET_BLINK_PAUSE_MS));
        paused || self.caret_visible
    }
}

impl EventEmitter<InputEvent> for InputState {}

impl Focusable for InputState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// -------------------------------------------------------------------------
// 文本输入（IME）——单行，整行是一个 UTF-16 区段
// -------------------------------------------------------------------------

/// 共享 IME 状态机的 byte 语义宿主适配层：内部坐标本来就是 byte，`&str` 遍历
/// 做 UTF-16 换算（单行框文本小，无需 rope 索引），落地经 `edit_range`（发
/// `Change`）。**跟踪 marked**（`supports_marked = true`）：Windows 组合更新
/// `range=None` 落当前 marked/选区，组合更新替换旧组合、提交整体落地（修中文
/// IME「逐键追加」乱码）；组合内偏移只摆光标、不建选区（`set_composition_caret`
/// 覆写），单行框仍不画组合下划线。
impl ImeTextHost for InputState {
    fn len_utf16(&self) -> usize {
        ime::utf16_len(self.text.as_str())
    }

    fn byte_to_utf16(&self, byte: usize) -> usize {
        ime::byte_to_utf16(self.text.as_str(), byte)
    }

    fn utf16_to_byte(&self, offset_utf16: usize) -> usize {
        ime::utf16_to_byte(self.text.as_str(), offset_utf16)
    }

    fn text_in_bytes(&self, range: Range<usize>) -> Option<String> {
        self.text.get(range).map(str::to_string)
    }

    fn selection_bytes(&self) -> Range<usize> {
        self.selection.clone()
    }

    /// 是否反选（`head < anchor`），驱动 `selected_text_range.reversed`。
    fn selection_reversed(&self) -> bool {
        self.selection.start < self.selection.end && self.selection_head == self.selection.start
    }

    fn set_selection_bytes(&mut self, range: Range<usize>) {
        self.selection = range.clone();
        self.selection_head = range.end;
    }

    fn marked_bytes(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn set_marked_bytes(&mut self, range: Option<Range<usize>>) {
        self.marked = range;
    }

    fn supports_marked(&self) -> bool {
        true
    }

    /// 组合内偏移只摆光标、不建选区（与 WYSIWYG 输入一致）：`None`（Windows）光标
    /// 落组合末尾，`Some(sel)`（macOS）光标落 sel.start。单行框不显示组合选区。
    fn set_composition_caret(
        &mut self,
        base_byte: usize,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
    ) {
        let caret = match new_selected_range {
            Some(sel) => base_byte + ime::utf16_to_byte(new_text, sel.start),
            None => base_byte + new_text.len(),
        };
        self.selection = caret.min(self.text.len())..caret.min(self.text.len());
        self.selection_head = caret.min(self.text.len());
    }

    fn apply_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        intent: EditIntent,
        cx: &mut Context<Self>,
    ) {
        // 撤销分类：ime「组合更新」= Atomic（apply_edit 里 host 不产生撤销单元，见
        // edit_range 的 ImeCompose 分支）；「组合提交/直输」= Typing，其中**组合中提交**
        // （marked 还在组合里）独立一组（Atomic），普通直输可内嵌合并。
        let class = match intent {
            EditIntent::Atomic => UndoClass::ImeCompose,
            EditIntent::Typing if self.marked.is_some() => UndoClass::Atomic,
            _ => UndoClass::Inline,
        };
        self.edit_range(range, text, class, cx);
    }
}

impl EntityInputHandler for InputState {
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        true
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.len_utf16())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(ime::selected_text_range(self))
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        ime::marked_text_range(self)
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        ime::text_for_range(self, range_utf16)
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ime::replace(self, range, text, None, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组合更新：`None` 区间 = 替换当前 marked/选区（共享核心跟踪 marked——
        // 不再逐键追加）；组合内光标经 `set_composition_caret` 只摆光标不建选区。
        ime::replace(self, range, new_text, new_selected_range, true, cx);
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        ime::unmark(self);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let byte = ime::utf16_to_byte(self.text.as_str(), range_utf16.start.min(range_utf16.end));
        let x = self.caret_px(byte, element_bounds, window);
        let line_h = self.size.line_height();
        Some(Bounds::new(
            point(x, element_bounds.origin.y + (element_bounds.size.height - line_h) / 2.),
            size(px(CARET_PX), line_h),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let x = (point.x - self.element_bounds.origin.x - px(PAD_PX)).max(px(0.));
        Some(ime::byte_to_utf16(
            self.text.as_str(),
            self.index_at_x_impl(x, window),
        ))
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ime::set_selected_text_range(self, range_utf16);
        // 平台层落定选区 = 用户光标/选区变更 → 撤销合并断层。
        self.cooperative = false;
        cx.notify();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.do_paste(&text, cx);
        }
    }

    fn text_input_configuration(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> TextInputConfiguration {
        TextInputConfiguration::default()
    }
}

// -------------------------------------------------------------------------
// 渲染：paint 期挂输入句柄 + 画文本/占位符/光标
// -------------------------------------------------------------------------

/// 标记类型：保持原 gpui_component API 形状，`Input::new(&state)` 返回实体（视图即元素）。
pub struct Input;

impl Input {
    #[allow(clippy::new_ret_no_self)] // API 兼容垫片：返回的就是传入的实体，非新建
    pub fn new(state: &Entity<InputState>) -> Entity<InputState> {
        state.clone()
    }
}

impl Render for InputState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首次渲染注册焦点事件（blink 启动/失焦复位），并做启动期焦点态对齐
        // （对齐编辑器 sync_render_state：window.focus() 若发生在注册前，事件会丢）。
        if !self.focus_registered {
            let _ = cx.on_focus(&self.focus_handle, window, Self::on_focus);
            let _ = cx.on_blur(&self.focus_handle, window, Self::on_blur);
            self.focus_registered = true;
        }
        if self.focus_handle.is_focused(window) && !self.focused {
            self.focused = true;
            self.kick_caret();
            if !self.blink_running {
                self.start_blink(cx);
            }
            cx.notify();
        } else if !self.focus_handle.is_focused(window) && self.focused {
            self.focused = false;
            self.cancel_mouse_drag();
            cx.notify();
        }
        let inner = div()
            .id("ui-input-box")
            .key_context("Input")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_prev_word))
            .on_action(cx.listener(Self::on_delete_next_word))
            .on_action(cx.listener(Self::on_move_left))
            .on_action(cx.listener(Self::on_move_right))
            .on_action(cx.listener(Self::on_move_home))
            .on_action(cx.listener(Self::on_move_end))
            .on_action(cx.listener(Self::on_move_to_start))
            .on_action(cx.listener(Self::on_move_to_end))
            .on_action(cx.listener(Self::on_move_prev_word))
            .on_action(cx.listener(Self::on_move_next_word))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_select_prev_word))
            .on_action(cx.listener(Self::on_select_next_word))
            .on_action(cx.listener(Self::on_select_to_start))
            .on_action(cx.listener(Self::on_select_to_end))
            .on_action(cx.listener(Self::on_select_to_start_of_line))
            .on_action(cx.listener(Self::on_select_to_end_of_line))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up_out))
            .w_full()
            .h(px(self.size.height_px()))
            .overflow_hidden();
        InputElement {
            entity: cx.entity(),
            inner: inner.into_any_element(),
        }
    }
}

/// 包装元素：内部 UI 走 gpui 布局，paint 期挂 `handle_input` 并画文本/占位符/光标。
struct InputElement {
    entity: Entity<InputState>,
    inner: AnyElement,
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("ui-input".into()))
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        base::util::source_location()
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.inner.request_layout(window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.entity.update(cx, |state, _| state.element_bounds = bounds);
        self.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.entity.read(cx).focus_handle(cx);
        window.handle_input(
            &focus,
            gpui::ElementInputHandler::new(bounds, self.entity.clone()),
            cx,
        );
        self.inner.paint(window, cx);

        let (focused, text, placeholder, selection, selection_head, caret_shown) = {
            let state = self.entity.read(cx);
            (
                state.focused(window, cx),
                state.text.clone(),
                state.placeholder.clone(),
                state.selection.clone(),
                state.selection_head,
                state.caret_shown_now(),
            )
        };
        let theme = Theme::global(cx).clone();
        let show_placeholder = text.is_empty() && !focused;
        let display: &str = if show_placeholder {
            placeholder.as_str()
        } else {
            text.as_str()
        };
        let injected = self.entity.read(cx).text_color;
        let fg: Hsla = injected.unwrap_or(theme.colors.fg.into());
        let color: Hsla = if show_placeholder {
            injected
                .map(|c| c.opacity(PLACEHOLDER_ALPHA))
                .unwrap_or(theme.colors.fg_muted.into())
        } else {
            fg
        };
        let fs = px(self.entity.read(cx).size.font_size_px());
        let run = TextRun {
            len: display.len(),
            font: gpui::font(SharedString::from(".SystemUIFont")),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line_h = self.entity.read(cx).size.line_height();
        // 水平滚动：聚焦且显示真实文本时，按光标（selection_head 端）必须落在可见带内
        // 重算 scroll_x 并写回实体（放 paint 期，避免把 `&mut Window` 穿进每个移动光标
        // 的方法）；本帧绘制用本地算出的值（避免双读）。非聚焦/空文本/占位符 → 0。
        let scroll_x = {
            let state = self.entity.read(cx);
            let caret_byte = selection_head.min(text.len());
            // 光标框内偏移 = 左内边距 + 前缀宽（未加 scroll 的自然位置）。
            let caret_offset = PAD_PX + state.text_width(&text[..caret_byte], window).as_f32();
            if focused && !show_placeholder && !text.is_empty() {
                let visible_w = bounds.size.width.as_f32();
                // 右缘留白 = 光标宽 + 2px；左缘留白 = 左内边距。
                let right_margin = CARET_PX + 2.0;
                let left_margin = PAD_PX;
                let mut s = state.scroll_x;
                if caret_offset > s + visible_w - right_margin {
                    // 光标超出右缘 → 右滚到光标停右缘。
                    s = caret_offset - (visible_w - right_margin);
                } else if caret_offset < s + left_margin {
                    // 光标越过左缘 → 左滚到光标停左缘。
                    s = (caret_offset - left_margin).max(0.0);
                }
                // 上界：光标到末尾时全文尾巴完全可见、右侧不留空白。
                let max_s = (state.text_width(&text, window).as_f32() + 2.0 * PAD_PX - visible_w)
                    .max(0.0);
                s.clamp(0.0, max_s)
            } else {
                0.0
            }
        };
        self.entity.update(cx, |s, _| s.scroll_x = scroll_x);
        // 文本/选区/光标真实绘制整体左移 scroll_x，并包进内容蒙板裁到框内；选区两端
        // 与光标都基于同一 text_origin 继续加前缀宽，不重复减 scroll。
        let text_origin = point(
            bounds.origin.x + px(PAD_PX) - px(scroll_x),
            bounds.origin.y + (bounds.size.height - line_h) / 2.,
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if !display.is_empty() {
                let line = window
                    .text_system()
                    .shape_line(SharedString::from(display.to_string()), fs, &[run], None);
                let _ = line.paint(text_origin, line_h, gpui::TextAlign::Left, None, window, cx);
            }
            // 选区背景（拖选/键选时高亮，对齐编辑器选区底色）+ 光标：仅聚焦且显示真实
            // 文本（非占位符）时。光标画在选区 head 端、由闪烁驱动显示。
            if focused && !show_placeholder {
                if selection.start < selection.end {
                    let s_x = text_origin.x
                        + self.entity.read(cx).text_width(&text[..selection.start], window);
                    let e_x = text_origin.x
                        + self.entity.read(cx).text_width(&text[..selection.end], window);
                    let sel_bg = theme.colors.editor_selection_bg;
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(point(s_x, text_origin.y), size(e_x - s_x, line_h)),
                        corner_radii: gpui::Corners::default(),
                        background: sel_bg.into(),
                        border_widths: gpui::Edges::default(),
                        border_color: sel_bg.into(),
                        border_style: gpui::BorderStyle::Solid,
                    });
                }
                if caret_shown {
                    let caret_byte = selection_head.min(text.len());
                    let prefix = &text[..caret_byte];
                    let caret_x = if prefix.is_empty() {
                        text_origin.x
                    } else {
                        text_origin.x + self.entity.read(cx).text_width(prefix, window)
                    };
                    window.paint_quad(gpui::PaintQuad {
                        bounds: Bounds::new(
                            point(caret_x, text_origin.y),
                            size(px(CARET_PX), line_h),
                        ),
                        corner_radii: gpui::Corners::default(),
                        background: fg.into(),
                        border_widths: gpui::Edges::default(),
                        border_color: fg,
                        border_style: gpui::BorderStyle::Solid,
                    });
                }
            }
        });
    }
}

// UTF-16 换算辅助（utf16_len / byte_to_utf16 / utf16_to_byte）已收敛到
// `ime`——见文件头与 [`ime::ImeTextHost`] 适配层。

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, ParentElement as _, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    struct InputHost {
        input: Entity<InputState>,
        _sub: gpui::Subscription,
    }
    impl Render for InputHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.input.clone())
        }
    }

    /// 带「祖先 on_key_down 记录器」的宿主：断言组件 `stop_propagation` 后祖先不再收到
    /// Enter/Esc。祖先 `on_key_down` 在输入框元素的祖先 dispatch 节点上。
    struct AncestorHost {
        input: Entity<InputState>,
        _sub: gpui::Subscription,
        ancestor: Rc<RefCell<usize>>,
    }
    impl Render for AncestorHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let anc = self.ancestor.clone();
            div()
                .on_key_down(move |_: &KeyDownEvent, _, _| *anc.borrow_mut() += 1)
                .child(self.input.clone())
        }
    }

    /// 窗口宿主：div 包裹输入框（Render 走 InputElement），订阅事件计数。
    fn add_input<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (
        Entity<InputState>,
        &'a mut VisualTestContext,
        Rc<RefCell<(usize, usize, usize)>>,
    ) {
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let events = Rc::new(RefCell::new((0usize, 0usize, 0usize)));
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索…")
                    .default_value(text)
            });
            let sub = events.clone();
            let _sub = cx.subscribe(&input, move |_, _, ev: &InputEvent, _| {
                let mut e = sub.borrow_mut();
                match ev {
                    InputEvent::Change => e.0 += 1,
                    InputEvent::Submit { .. } => e.1 += 1,
                    InputEvent::Cancel => e.2 += 1,
                }
            });
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        (holder.borrow().clone().unwrap(), wcx, events)
    }

    /// `default_value`/`value`/占位符 + `set_value` 发 Change。
    #[gpui::test]
    async fn input_value_and_change_emission(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "abc");
        let got = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).placeholder.clone().to_string(),
            )
        });
        assert_eq!(got.0, "abc");
        assert_eq!(got.1, "搜索…");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| s.set_value("你好 world", window, cx));
        });
        let value = wcx.update(|_w, cx| input.read(cx).value().to_string());
        assert_eq!(value, "你好 world");
        assert_eq!(changes.borrow().0, 1, "set_value 应发出一次 Change");
    }

    /// Enter/Esc 统一收敛为 Submit/Cancel 事件上抛（事件上抛、行为下取）：
    /// 焦点在输入框时按 Enter → 恰好一个 Submit（shift=false）且**祖先 on_key_down
    /// 不被调用**（组件已 `stop_propagation`）；Shift+Enter → Submit{shift=true}；
    /// Esc → 恰好一个 Cancel 且祖先不被调用。这是「仅收到一个 Submit」的收敛断言
    /// （对应 gpui `stop_propagation/prevent_default/action` 派发次序的实测定性：
    /// key_down 层的 stop 拦截祖先 key_down；若祖先链还有 `on_action(Enter/Escape)`，
    /// 动作层先于 KeyDownEvent 派发、本组件 stop 拦不住——见 WYSIWYG 图片卡泄漏项）。
    #[gpui::test]
    async fn input_enter_escape_submit_cancel_stops_at_component(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let events = Rc::new(RefCell::new((0usize, 0usize, 0usize)));
        let last_shift = Rc::new(RefCell::new(None::<bool>));
        let ancestor_hits = Rc::new(RefCell::new(0usize));
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索…")
                    .default_value("abc")
            });
            let sub = events.clone();
            let shift_cell = last_shift.clone();
            let _sub = cx.subscribe(&input, move |_, _, ev: &InputEvent, _| {
                let mut e = sub.borrow_mut();
                match ev {
                    InputEvent::Change => e.0 += 1,
                    InputEvent::Submit { shift } => {
                        e.1 += 1;
                        *shift_cell.borrow_mut() = Some(*shift);
                    }
                    InputEvent::Cancel => e.2 += 1,
                }
            });
            let anc = ancestor_hits.clone();
            let anc_host = AncestorHost {
                input,
                _sub,
                ancestor: anc,
            };
            *holder.borrow_mut() = Some(anc_host.input.clone());
            anc_host
        });
        let _ = _root;
        let input = holder.borrow().clone().unwrap();
        settle_window(wcx);
        let w = wcx.windows().first().copied().expect("有测试窗口");
        // 聚焦输入框（真实键盘路径起点）。
        wcx.update(|window, cx| {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        });
        wcx.run_until_parked();
        // Enter → 恰好一个 Submit（shift=false），祖先 key_down 不触发。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("enter").unwrap());
        assert_eq!(*events.borrow(), (0, 1, 0), "Enter 应恰好发一个 Submit");
        assert_eq!(
            *last_shift.borrow(),
            Some(false),
            "普通 Enter 的 Submit.shift 应为 false"
        );
        assert_eq!(*ancestor_hits.borrow(), 0, "Enter 不应冒泡到祖先 on_key_down");
        // Shift+Enter → Submit{shift=true}。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("shift-enter").unwrap());
        assert_eq!(*events.borrow(), (0, 2, 0), "Shift+Enter 应再发一个 Submit");
        assert_eq!(
            *last_shift.borrow(),
            Some(true),
            "Shift+Enter 的 Submit.shift 应为 true"
        );
        assert_eq!(*ancestor_hits.borrow(), 0, "Shift+Enter 不应冒泡到祖先 on_key_down");
        // Esc → 恰好一个 Cancel，祖先 key_down 不触发。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("escape").unwrap());
        assert_eq!(*events.borrow(), (0, 2, 1), "Esc 应恰好发一个 Cancel");
        assert_eq!(*ancestor_hits.borrow(), 0, "Esc 不应冒泡到祖先 on_key_down");
    }

    /// `replace_text_in_range`（IME/直输）正确落点 + 发 Change + 中文 UTF-16 换算。
    #[gpui::test]
    async fn input_ime_replace_lands_at_utf16(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "abcde");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(1..4), "X", window, cx);
            });
        });
        assert_eq!(
            wcx.update(|_w, cx| input.read(cx).value().to_string()),
            "aXe"
        );
        assert_eq!(changes.borrow().0, 1);
        // 中文：utf16[2] 是第二个字符后（"中文"后），字节偏移 ≠ utf16
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| s.set_value("中文测试", window, cx));
        });
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(2..2), "！", window, cx);
            });
        });
        assert_eq!(
            wcx.update(|_w, cx| input.read(cx).value().to_string()),
            "中文！测试"
        );
    }

    /// IME 组合路径回归（M2.5 收敛护栏 + 中文 IME 修复）：单行输入框跟踪 marked，
    /// `None` 区间 = 替换当前 marked/选区——组合更新**替换旧组合不追加**（修中文
    /// IME「逐键追加」乱码，与编辑器/WYSIWYG 一致）；显式 UTF-16 区间精确替换；
    /// 组合内偏移只摆光标；提交（普通 `replace_text_in_range`）整体落地并结束组合。
    #[gpui::test]
    async fn input_ime_composition_treated_as_replace(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "ab");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                // None 区间 = 当前 marked/选区：无 marked → 光标处插入
                s.replace_and_mark_text_in_range(None, "中", None, window, cx);
                // 第二次组合更新仍 None：替换旧 marked（不追加！）
                s.replace_and_mark_text_in_range(None, "文", None, window, cx);
                // 显式 UTF-16 区间（macOS 风格 marked range）：精确替换 utf16[1..4]
                // （=“b文”，含代理对粒度换算，非字节偏移）
                s.replace_and_mark_text_in_range(Some(1..4), "国", None, window, cx);
                // 提交：普通替换落当前 marked 并结束组合（marked 清理在核心统一处理）
                s.replace_text_in_range(None, "界", window, cx);
            });
        });
        let (value, caret) = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).selection.clone(),
            )
        });
        // 轨迹：ab → ab中（marked 替换）→ ab文 → a国（utf16[1..4]=“b文”被“国”替换）
        // → a界（提交替换 marked）
        assert_eq!(value, "a界");
        assert_eq!(caret, "a界".len().."a界".len(), "光标停在提交文本末尾");
        assert_eq!(changes.borrow().0, 4, "组合每步与提交各发一次 Change");
    }

    /// 复现：侧边栏搜索框中文 IME「逐键追加乱码」——用户输「编辑」输入框出现
    /// 「bbibi'abianbian'jbian'ji编辑」。Windows IME 组合更新走
    /// `replace_and_mark_text_in_range(None, 组合串, …)`、提交走
    /// `replace_text_in_range(None, 已提交中文)`（见 gpui_windows `handle_ime_composition_inner`）。
    /// `None` 区间 = 当前 marked/选区：若输入框不跟踪 marked，每键组合**追加**到光标
    /// 后而非替换旧组合，提交又拼一段中文。断言：组合替换不追加、提交后只剩中文。
    #[gpui::test]
    async fn input_ime_chinese_composition_does_not_stack(cx: &mut TestAppContext) {
        let (input, wcx, changes) = add_input(cx, "");
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                // Windows IME 组合更新序列：每键重发**完整**组合串（range=None）
                s.replace_and_mark_text_in_range(None, "b", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'a", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bi'an", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'j", None, window, cx);
                s.replace_and_mark_text_in_range(None, "bian'ji", None, window, cx);
                // 确认：提交中文（range=None），组合应被整体替换
                s.replace_text_in_range(None, "编辑", window, cx);
            });
        });
        let (value, caret) = wcx.update(|_w, cx| {
            (
                input.read(cx).value().to_string(),
                input.read(cx).selection.clone(),
            )
        });
        assert_eq!(
            value, "编辑",
            "组合应逐键替换不追加，提交后只剩中文（而非拼音历史拼接）"
        );
        assert_eq!(caret, "编辑".len().."编辑".len(), "光标停在提交文本末尾");
        assert_eq!(changes.borrow().0, 7, "组合每步与提交各发一次 Change");
    }

    /// 跑几帧，等渲染/layout/prepaint 就绪（element_bounds 供鼠标命中换算）。
    fn settle_window(wcx: &mut VisualTestContext) {
        for _ in 0..3 {
            let _ = wcx.refresh();
            wcx.run_until_parked();
        }
    }

    /// 复现 1：侧边栏搜索框「无法按住鼠标拖动选中文字」——`on_mouse_down` 只摆光标、
    /// 没有线内 mouse-move 拖动扩展选区。断言：按下左键后拖动（移动鼠标）应形成选区。
    #[gpui::test]
    async fn mouse_drag_selects_range(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "hello world");
        settle_window(wcx);
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        // 按下：文本起始处（元素左缘 + 左内边距）→ 光标落 0
        let down = point(bounds.origin.x + px(PAD_PX), y);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: down,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "按下落点应先把光标坍缩到 0"
        );
        // 按住左键拖到元素右缘 → 选区应扩展（而非仍坍缩）
        let drag_to = point(bounds.origin.x + bounds.size.width - px(1.), y);
        wcx.simulate_event(gpui::MouseMoveEvent {
            position: drag_to,
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        });
        let sel = wcx.update(|_, cx| input.read(cx).selection.clone());
        assert_eq!(
            sel,
            0.."hello world".len(),
            "按住左键拖到右缘应形成 0..len 选区（拖选）"
        );
        // 反向拖回落点 → 选区收缩坍缩（锚点固定）
        wcx.simulate_event(gpui::MouseMoveEvent {
            position: down,
            pressed_button: Some(gpui::MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "拖回落点应收缩到坍缩"
        );
        // 松开左键 → 拖拽闭环
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: down,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        assert!(
            !wcx.update(|_, cx| input.read(cx).dragging),
            "松开后拖拽态应复位"
        );
    }

    /// 复现 2：搜索框「Ctrl+← / Ctrl+→ 无法扩选词」。keymap 已绑
    /// `ctrl-shift-left/right` → `SelectToPreviousWordStart/SelectToNextWordEnd`（见
    /// `base::actions::bind_input_keys`），但 input.rs 没有对应 `on_action` handler。
    /// 断言：Ctrl+Shift+方向应扩选词；普通 Ctrl+方向仍只移动光标（回归护栏）。
    #[gpui::test]
    async fn ctrl_shift_arrow_selects_word(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "foo bar baz");
        settle_window(wcx);
        // 点按聚焦并落光标到文本首
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "点按文本首应坍缩光标到 0"
        );
        let w = wcx.windows().first().copied().expect("有测试窗口");
        // Ctrl+Shift+→：扩选第一个词 foo（0..3）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..3,
            "Ctrl+Shift+→ 应扩选到词尾（foo）"
        );
        // 再按：扩到 "foo bar"（0..7）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..7,
            "连续 Ctrl+Shift+→ 应扩展到第二个词尾"
        );
        // Ctrl+Shift+←：收缩一个词（head 7 → 4）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-shift-left").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..4,
            "Ctrl+Shift+← 应收缩到词头"
        );
        // 普通 Ctrl+→：仅移动光标（不扩选）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            7..7,
            "普通 Ctrl+→ 应只移动光标"
        );
    }

    /// 复现 2b：Shift+← / Shift+→ 逐字符扩选（与主编辑器 on_key_down 同套路——
    /// `Select{Left,Right}` 无本地 action，走原样 key_down 层）。
    #[gpui::test]
    async fn shift_arrow_selects_by_char(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "abcd");
        settle_window(wcx);
        let bounds = wcx.update(|window, cx| {
            let b = input.read(cx).element_bounds.clone();
            let _ = window;
            b
        });
        let y = bounds.origin.y + px(4.);
        wcx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        wcx.simulate_event(gpui::MouseUpEvent {
            button: gpui::MouseButton::Left,
            position: point(bounds.origin.x + px(PAD_PX), y),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        });
        let w = wcx.windows().first().copied().expect("有测试窗口");
        // 先三个右移 → 光标 3
        for _ in 0..3 {
            wcx.dispatch_keystroke(w, gpui::Keystroke::parse("right").unwrap());
        }
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..3,
            "右移三次光标应在 3"
        );
        // Shift+Right：扩到 3..4
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("shift-right").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..4,
            "Shift+Right 应扩选一个字符"
        );
        // Shift+Left：收缩回 3..3
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("shift-left").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            3..3,
            "Shift+Left 应收缩一个字符"
        );
    }

    /// 复现 3：搜索框「光标不闪烁」——聚焦后应启动 blink 循环、光标先可见；暂停期
    /// 结束后可见性随计时翻转（闪烁）；失焦后循环退出不再翻转。
    #[gpui::test]
    async fn caret_blinks_with_timer(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "ab");
        // 未聚焦：光标不画
        assert!(
            !wcx.update(|_, cx| input.read(cx).caret_shown_now()),
            "未聚焦光标应不可见"
        );
        // 聚焦 → 光标可见（静止期开始）、blink 循环启动
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let (shown, running, visible) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.caret_shown_now(), s.blink_running, s.caret_visible)
        });
        assert!(shown, "聚焦后光标应可见");
        assert!(running, "聚焦后应启动 blink 循环");
        assert!(visible, "聚焦后 caret_visible 应为 true");
        // 模拟静止期结束（无暂停标记 → 循环下一次 tick 翻转），推进虚拟时钟驱动计时器
        wcx.update(|_, cx| input.update(cx, |s, _| s.caret_pause_until = None));
        wcx.executor()
            .advance_clock(Duration::from_millis(CARET_BLINK_MS + 10));
        wcx.run_until_parked();
        let after = wcx.update(|_, cx| input.read(cx).caret_visible);
        assert!(!after, "光标可见性应随 blink 计时翻转（闪烁）");
        // 失焦：真正 blur 窗口焦点态 + 直调 on_blur（gpui 测试环境不派发真实 focus/blur
        // 事件，与编辑器 active_line_bg_requires_focus 的「focused=false 分支直测」同套路）。
        // 不真正清焦点的话，render 焦点对齐会把 `focused` 拉回 true。
        wcx.update(|window, cx| {
            window.blur(cx);
            input.update(cx, |s, cx| s.on_blur(window, cx));
        });
        // 失焦后循环还要再睡满周期才到退出判定（toggle → sleep → keep-check），分拍
        // 推进 + 每拍 park（对齐本仓 pdf/editor 的 advance_clock 驱动惯用法）。
        for _ in 0..8 {
            wcx.executor()
                .advance_clock(Duration::from_millis(CARET_BLINK_MS));
            wcx.run_until_parked();
        }
        let (running2, shown2) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.blink_running, s.caret_shown_now())
        });
        assert!(!running2, "失焦后 blink 循环应退出");
        assert!(!shown2, "失焦后光标应不可见");
    }

    /// 长文本水平滚动：聚焦后 paint 期按光标（`default_value` 把光标落文本末尾）重算
    /// `scroll_x>0` 使光标落入可见带内（锁真实对位量：光标 x ∈ 框内）；短文本聚焦时
    /// `scroll_x==0`（无需滚动）。K138：锁真实对位量、不锁相对趋势。
    #[gpui::test]
    async fn long_text_scrolls_to_caret_when_focused(cx: &mut TestAppContext) {
        let long = "a".repeat(400);
        let (input, wcx, _) = add_input(cx, &long);
        settle_window(wcx);
        // 聚焦（不点按，光标仍在文本末尾）
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let (scroll_x, caret_x, min_x, max_x) = wcx.update(|window, cx| {
            let s = input.read(cx);
            let bounds = s.element_bounds;
            let caret = s.caret_px(long.len(), bounds, window);
            (
                s.scroll_x,
                caret.as_f32(),
                bounds.origin.x.as_f32(),
                (bounds.origin.x + bounds.size.width).as_f32(),
            )
        });
        assert!(scroll_x > 0.0, "长文本聚焦后应滚动（scroll_x={}）", scroll_x);
        assert!(
            caret_x >= min_x && caret_x <= max_x,
            "光标 x 应落在框内 [{}, {}]，实际 {}",
            min_x,
            max_x,
            caret_x
        );
    }

    /// 短文本聚焦无需滚动：`scroll_x` 保持 0（光标在框内无溢出时不该滚）。
    #[gpui::test]
    async fn short_text_focus_keeps_scroll_zero(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "ab");
        settle_window(wcx);
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let scroll_x = wcx.update(|_, cx| input.read(cx).scroll_x);
        assert_eq!(scroll_x, 0.0, "短文本聚焦不应滚动");
    }

    /// 命中换算同步：滚动后左缘（框内坐标 = PAD_PX）对应的不是第 0 个字符，而是滚动
    /// 偏移处的字符——`index_at_x_impl` 内部已加回 `scroll_x`，保证「点击的字符」与
    /// 「屏幕可见的字符」一致（on_mouse_down / character_index_for_point 都经它）。
    #[gpui::test]
    async fn long_text_hit_test_maps_with_scroll(cx: &mut TestAppContext) {
        let long = "a".repeat(400);
        let (input, wcx, _) = add_input(cx, &long);
        settle_window(wcx);
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let (scroll_x, idx) = wcx.update(|window, cx| {
            let s = input.read(cx);
            (s.scroll_x, s.index_at_x_impl(px(PAD_PX), window))
        });
        assert!(scroll_x > 0.0, "前置：长文本聚焦应已滚动");
        assert!(idx > 0, "左缘可见应不是第 0 个字符（已滚动到中部），实际 {}", idx);
    }

    /// 失焦复位滚动：聚焦长文本滚动后失焦 → `scroll_x` 归 0（显示头部；再次聚焦由
    /// paint 期按光标重滚）。
    #[gpui::test]
    async fn blur_resets_scroll(cx: &mut TestAppContext) {
        let long = "a".repeat(400);
        let (input, wcx, _) = add_input(cx, &long);
        settle_window(wcx);
        wcx.update(|window, cx| window.focus(&input.read(cx).focus_handle(cx), cx));
        settle_window(wcx);
        let scrolled = wcx.update(|_, cx| input.read(cx).scroll_x);
        assert!(scrolled > 0.0, "前置：聚焦后应已滚动");
        wcx.update(|window, cx| {
            window.blur(cx);
            input.update(cx, |s, cx| s.on_blur(window, cx));
        });
        let after_blur = wcx.update(|_, cx| input.read(cx).scroll_x);
        assert_eq!(after_blur, 0.0, "失焦应复位 scroll_x 到 0");
    }

    // ---------------------------------------------------------------------
    // P3/D1 声明式参数面守护测试（先 Red 后 Green）
    // ---------------------------------------------------------------------

    /// 按尺寸档建输入框并测量布局高度（settle 后取 element_bounds）。
    fn input_height(cx: &mut TestAppContext, size: InputSize) -> f32 {
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).size(size));
            let _sub = cx.subscribe(&input, |_, _, _: &InputEvent, _| {});
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        let input = holder.borrow().clone().unwrap();
        settle_window(wcx);
        wcx.update(|_, cx| input.read(cx).element_bounds.size.height.as_f32())
    }

    /// P3/D1 A：尺寸档 — Compact/Regular/Large 产生不同布局高度
    /// （Regular 保持现状 h24；断言 element_bounds 高度非趋势）。
    #[gpui::test]
    async fn input_size_tiers_yield_distinct_heights(cx: &mut TestAppContext) {
        let compact = input_height(cx, InputSize::Compact);
        let regular = input_height(cx, InputSize::Regular);
        let large = input_height(cx, InputSize::Large);
        assert_eq!(regular, 24.0, "Regular 应保持现状高度 24，实际 {}", regular);
        assert_ne!(compact, regular, "Compact 高度应不同于 Regular");
        assert_ne!(large, regular, "Large 高度应不同于 Regular");
        assert_ne!(compact, large, "Compact 高度应不同于 Large");
    }

    /// 按粘贴模式建输入框（P3/D1 粘贴测试用）。
    fn add_input_paste<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
        paste_mode: PasteMode,
    ) -> (Entity<InputState>, &'a mut VisualTestContext) {
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(text)
                    .paste_mode(paste_mode)
            });
            let _sub = cx.subscribe(&input, |_, _, _: &InputEvent, _| {});
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        (holder.borrow().clone().unwrap(), wcx)
    }

    /// P3/D1 B：StripNewlines — 粘贴 "a\r\nb\nc" → "a b c"（每个换行折叠为单个空格）。
    #[gpui::test]
    async fn paste_strip_newlines_folds_newlines(cx: &mut TestAppContext) {
        let (input, wcx) = add_input_paste(cx, "base", PasteMode::StripNewlines);
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.paste(gpui::ClipboardItem::new_string("a\r\nb\nc".to_string()), window, cx)
            });
        });
        let value = wcx.update(|_, cx| input.read(cx).value().to_string());
        assert_eq!(value, "basea b c", "换行应折叠为单个空格后插入，实际 {:?}", value);
    }

    /// P3/D1 C：RejectMultiline — 含换行的粘贴整段拒绝（不插入）；单行粘贴照常。
    #[gpui::test]
    async fn paste_reject_multiline_blocks_folded_text(cx: &mut TestAppContext) {
        let (input, wcx) = add_input_paste(cx, "ab", PasteMode::RejectMultiline);
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.paste(gpui::ClipboardItem::new_string("x\ny".to_string()), window, cx)
            });
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "ab",
            "含换行粘贴应整段拒绝（不插入）"
        );
        // 单行粘贴不受影响 → 正常插入
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.paste(gpui::ClipboardItem::new_string("z".to_string()), window, cx)
            });
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "abz",
            "单行粘贴应照常插入"
        );
    }

    /// P3/D1 D：默认路径保全 — 不加 paste_mode = Passthrough（文本原样插入，
    /// 不折叠不拒绝）。注：单行框 render 走 `shape_line`，含 `\n` 的文本会触发 gpui
    /// "text should not contain newlines" panic（既有限制，非本批引入），故此处用
    /// 无换行文本断言「默认不折叠不拒绝」。
    #[gpui::test]
    async fn paste_default_passthrough_preserves_text(cx: &mut TestAppContext) {
        let (input, wcx) = add_input_paste(cx, "ab", PasteMode::default());
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.paste(gpui::ClipboardItem::new_string("xy".to_string()), window, cx)
            });
        });
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "abxy",
            "默认粘贴应原样插入（现状行为）"
        );
    }

    /// D1 fix：Passthrough + 含换行粘贴 —— 单行不变式把换行折叠为空格，**不得**让
    /// `\n` 落进 `text`（否则 render/`shape_line`/`text_width`/`index_at_x` 全链路 panic，
    /// `vendor/text_system.rs:645` debug_assert）。折叠后继续可渲染、不 panic。
    #[gpui::test]
    async fn paste_default_passthrough_folds_newlines_into_single_line(cx: &mut TestAppContext) {
        let (input, wcx) = add_input_paste(cx, "ab", PasteMode::default());
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.paste(gpui::ClipboardItem::new_string("x\ny\nz".to_string()), window, cx)
            });
        });
        let value = wcx.update(|_, cx| input.read(cx).value().to_string());
        assert_eq!(value, "abx y z", "含换行粘贴应折叠为空格，实际 {:?}", value);
        assert!(
            !value.contains('\n'),
            "单行不变式：text 不得含换行，实际 {:?}",
            value
        );
        // 折叠不改字节长度 → 光标/选区算术不影响（值为 7 字符）。
        let (sel, head) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.selection.clone(), s.selection_head)
        });
        assert_eq!(sel, 7..7, "光标应收敛到末尾");
        assert_eq!(head, 7);
    }

    /// D1 fix：IME 组合替换路径同样受单行不变式约束（ime::replace → apply_edit →
    /// edit_range），组合文本带换行也不得落进 text。
    #[gpui::test]
    async fn ime_replace_folds_newlines_into_single_line(cx: &mut TestAppContext) {
        let (input, wcx) = add_input_paste(cx, "a", PasteMode::default());
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(1..1), "b\nc", window, cx);
            });
        });
        let value = wcx.update(|_, cx| input.read(cx).value().to_string());
        assert_eq!(value, "ab c", "IME 替换含换行应折叠为空格，实际 {:?}", value);
        assert!(!value.contains('\n'));
        // 组合标记路径（replace_and_mark_text_in_range）同源，也应折叠：
        let (input2, wcx2) = add_input_paste(cx, "a", PasteMode::default());
        wcx2.update(|window, cx| {
            input2.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(Some(1..1), "d\ne", None, window, cx);
            });
        });
        let value2 = wcx2.update(|_, cx| input2.read(cx).value().to_string());
        assert_eq!(value2, "ad e", "IME 组合含换行应折叠为空格，实际 {:?}", value2);
        assert!(!value2.contains('\n'));
    }

    /// P3/D1 测试校验器：非空才放行。
    fn validate_nonempty(text: &str, _window: &Window) -> Validation {
        if text.is_empty() {
            Validation::Err(Some("不能为空".into()))
        } else {
            Validation::Ok
        }
    }

    /// P3/D1 E：validate+confirm_required — invalid → Enter 不 emit Submit；
    /// valid → emit；且拦截不影响其它键（Esc 仍 Cancel）。
    #[gpui::test]
    async fn confirm_required_blocks_invalid_submit(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let events = Rc::new(RefCell::new((0usize, 0usize, 0usize)));
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value("hi")
                    .validate(validate_nonempty)
                    .confirm_required(true)
            });
            let sub = events.clone();
            let _sub = cx.subscribe(&input, move |_, _, ev: &InputEvent, _| {
                let mut e = sub.borrow_mut();
                match ev {
                    InputEvent::Change => e.0 += 1,
                    InputEvent::Submit { .. } => e.1 += 1,
                    InputEvent::Cancel => e.2 += 1,
                }
            });
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        let input = holder.borrow().clone().unwrap();
        settle_window(wcx);
        let w = wcx.windows().first().copied().expect("有测试窗口");
        wcx.update(|window, cx| {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        });
        wcx.run_until_parked();
        // 文本非空 → validate Ok → Enter 照常 emit Submit
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("enter").unwrap());
        assert_eq!(events.borrow().1, 1, "valid → Enter 应 emit Submit");
        // 置空文本 → validate Err → Enter 被拦截（不 emit Submit）
        wcx.update(|window, cx| input.update(cx, |s, cx| s.set_value("", window, cx)));
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("enter").unwrap());
        assert_eq!(events.borrow().1, 1, "invalid → Enter 不应再 emit Submit");
        // 拦截提交不影响其它键（Esc 仍 Cancel）
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("escape").unwrap());
        assert_eq!(events.borrow().2, 1, "拦截提交不影响其它键（Esc 仍 Cancel）");
    }

    /// P3/D1 F：confirm_required=false（默认）→ validate 返回 Err 时 Enter 也 emit
    /// Submit（默认保全断言：不设置 confirm_required 即现状行为）。
    #[gpui::test]
    async fn confirm_not_required_emits_even_invalid(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let holder: Rc<RefCell<Option<Entity<InputState>>>> = Rc::default();
        let events = Rc::new(RefCell::new((0usize, 0usize, 0usize)));
        let (_root, wcx) = cx.add_window_view(|window, cx| {
            // confirm_required 不设置 → 默认 false；validate 返回 Err 也不拦截
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value("")
                    .validate(validate_nonempty)
            });
            let sub = events.clone();
            let _sub = cx.subscribe(&input, move |_, _, ev: &InputEvent, _| {
                let mut e = sub.borrow_mut();
                match ev {
                    InputEvent::Change => e.0 += 1,
                    InputEvent::Submit { .. } => e.1 += 1,
                    InputEvent::Cancel => e.2 += 1,
                }
            });
            *holder.borrow_mut() = Some(input.clone());
            InputHost { input, _sub }
        });
        let _ = _root;
        let input = holder.borrow().clone().unwrap();
        settle_window(wcx);
        let w = wcx.windows().first().copied().expect("有测试窗口");
        wcx.update(|window, cx| {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        });
        wcx.run_until_parked();
        // 未设置 confirm_required：即使 text 为空（validate Err）Enter 仍 emit Submit
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("enter").unwrap());
        assert_eq!(
            events.borrow().1,
            1,
            "confirm_required=false 时 invalid 也应 emit Submit"
        );
    }

    // ---------------------------------------------------------------------
    // R1-R4：粘贴 / 剪切 / 撤销 / 重做
    // ---------------------------------------------------------------------

    /// 绑定 ui 本地键位（含 ctrl-x/z/y，见 `test_support`）+ 聚焦输入框，返回窗口句柄
    /// （供 `dispatch_keystroke` 走真实 按键→action 路径）。
    fn focus_input_for_keys(
        input: &Entity<InputState>,
        wcx: &mut VisualTestContext,
    ) -> gpui::AnyWindowHandle {
        let w = wcx.windows().first().copied().expect("有测试窗口");
        wcx.update(|window, cx| {
            let fh = input.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        });
        wcx.run_until_parked();
        w
    }

    /// 与 main.rs 同序：`bind_local_keymaps`（含 ctrl-x/z/y）**之后**补 `ctrl-v →
    /// PasteNormalized` 覆盖绑定（Input 键上下文），供粘贴测试走真实键链（与
    /// `shell/app/tests/preview.rs` 同款姿势）。
    fn bind_paste_normalized(cx: &TestAppContext) {
        cx.update(|cx| {
            crate::test_support::bind_local_keymaps(cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-v",
                PasteNormalized,
                Some("Input"),
            )]);
        });
    }

    /// 直调内嵌输入路径（`replace_text_in_range` = 真实键盘直输的 EntityInputHandler
    /// 入口，ASCII 下字节==UTF-16，测试样本全 ASCII）。每次在收拢光标处插入一段。
    fn type_text(input: &Entity<InputState>, text: &str, wcx: &mut VisualTestContext) {
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                let sel = s.selection.clone();
                s.replace_text_in_range(Some(sel.start..sel.start), text, window, cx);
            });
        });
    }

    /// R1：Ctrl+V 粘贴收口——真实键链：选区替换 + 粘贴整体一次撤销/重做。
    /// 期望推导：hello world 选区 6..11（"world"，字节 h0 e1 l2 l3 o4 空格5 w6…）
    /// 被 "X"（1 字节）替换 → "hello X"，光标 = 6 + 1 = 7。
    #[gpui::test]
    async fn paste_via_ctrl_v_replaces_selection_and_undoes_as_one_group(cx: &mut TestAppContext) {
        bind_paste_normalized(cx);
        let (input, wcx, changes) = add_input(cx, "hello world");
        settle_window(wcx);
        wcx.update(|_, cx| {
            input.update(cx, |s, _| {
                s.selection = 6..11;
                s.selection_head = 11;
            });
        });
        wcx.write_to_clipboard(gpui::ClipboardItem::new_string("X".to_string()));
        let w = focus_input_for_keys(&input, wcx);
        let before = changes.borrow().0;
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "hello X",
            "粘贴应替换选区 6..11（world）→ hello X"
        );
        let sel = wcx.update(|_, cx| input.read(cx).selection.clone());
        assert_eq!(sel, 7..7, "粘贴后光标收敛到新文本末尾（6+1=7）");
        assert_eq!(changes.borrow().0, before + 1, "粘贴应发一次 Change");
        // Ctrl+Z：整段粘贴作一次撤销单元整体回退。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "hello world",
            "粘贴应作为一次撤销单元整体撤销"
        );
        // Ctrl+Y 回推。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-y").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "hello X",
            "重做应恢复粘贴结果"
        );
    }

    /// R1：无选区粘贴 = 光标处插入；空剪贴板（无文本条目）不动作、不发 Change。
    /// 期望推导：a|b（光标 1）粘 "XY" → "aXYb"（1+2+1=4 字节）。
    #[gpui::test]
    async fn paste_inserts_at_caret_and_empty_clipboard_is_noop(cx: &mut TestAppContext) {
        bind_paste_normalized(cx);
        let (input, wcx, changes) = add_input(cx, "ab");
        settle_window(wcx);
        wcx.write_to_clipboard(gpui::ClipboardItem::new_string("XY".to_string()));
        let w = focus_input_for_keys(&input, wcx);
        wcx.update(|_, cx| {
            input.update(cx, |s, _| {
                s.selection = 1..1;
                s.selection_head = 1;
            });
        });
        let before = changes.borrow().0;
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "aXYb",
            "无选区粘贴应在光标 1 处插入"
        );
        assert_eq!(changes.borrow().0, before + 1, "粘贴应发一次 Change");
        // 真正「空剪贴板」：无任何条目的 ClipboardItem（`text()` == None）。
        wcx.write_to_clipboard(gpui::ClipboardItem { entries: vec![] });
        let before2 = changes.borrow().0;
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "aXYb",
            "空剪贴板（无文本）粘贴应无副作用"
        );
        assert_eq!(changes.borrow().0, before2, "空剪贴板粘贴不应发 Change");
    }

    /// R1+R5：PasteMode 经 Ctrl+V 路径同样生效——StripNewlines 折叠、RejectMultiline 整段
    /// 拒绝。期望推导：strip：base(4) + "a b c"(5) = "basea b c"（9 字节）。
    #[gpui::test]
    async fn paste_via_ctrl_v_applies_paste_mode(cx: &mut TestAppContext) {
        bind_paste_normalized(cx);
        // StripNewlines：a\r\nb\nc → 折叠为 a b c。
        let (strip, wcx) = add_input_paste(cx, "base", PasteMode::StripNewlines);
        settle_window(wcx);
        wcx.write_to_clipboard(gpui::ClipboardItem::new_string("a\r\nb\nc".to_string()));
        let w = focus_input_for_keys(&strip, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(
            wcx.update(|_, cx| strip.read(cx).value().to_string()),
            "basea b c",
            "StripNewlines 应折叠换行为单个空格（4+5=9 字节）"
        );
        // RejectMultiline：含换行整段拒绝（文本保持原样、不修改）。
        let (rej, wcx2) = add_input_paste(cx, "ab", PasteMode::RejectMultiline);
        settle_window(wcx2);
        wcx2.write_to_clipboard(gpui::ClipboardItem::new_string("x\ny".to_string()));
        let w2 = focus_input_for_keys(&rej, wcx2);
        wcx2.dispatch_keystroke(w2, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(
            wcx2.update(|_, cx| rej.read(cx).value().to_string()),
            "ab",
            "RejectMultiline 含换行粘贴应整段拒绝（不修改）"
        );
    }

    /// R2：Ctrl+X 剪切收口——非空选区：写剪贴板 + 删选区 + 一次撤销可复原。
    /// 期望推导：hello world 选区 0..5（"hello"，h0 e1 l2 l3 o4）→ 剪贴板 "hello"，
    /// 文本剩 " world"（下标 5 的空格保留，5..11 共 6 字节）。
    #[gpui::test]
    async fn cut_via_ctrl_x_writes_clipboard_and_undoes(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, changes) = add_input(cx, "hello world");
        settle_window(wcx);
        wcx.update(|_, cx| {
            input.update(cx, |s, _| {
                s.selection = 0..5;
                s.selection_head = 5;
            });
        });
        let w = focus_input_for_keys(&input, wcx);
        let before = changes.borrow().0;
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-x").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            " world",
            "剪切应删除选区 0..5（hello）"
        );
        assert_eq!(
            wcx.read_from_clipboard().and_then(|i| i.text()),
            Some("hello".to_string()),
            "剪贴板应写入被剪切的文本"
        );
        assert_eq!(changes.borrow().0, before + 1, "剪切应发一次 Change");
        // Ctrl+Z：剪切一次撤销复原。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "hello world",
            "剪切应可一次撤销复原"
        );
    }

    /// R2：空选区剪切 = 无动作（不采纳编辑器「空选区复制整行」惯例）——文本不变、
    /// 剪贴板不被改写、不发 Change。
    #[gpui::test]
    async fn cut_empty_selection_is_noop(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, changes) = add_input(cx, "ab");
        settle_window(wcx);
        // 预置剪贴板为已知值，断言空选区剪切不会改写它。
        wcx.write_to_clipboard(gpui::ClipboardItem::new_string("KEEP".to_string()));
        let w = focus_input_for_keys(&input, wcx);
        let before = changes.borrow().0;
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-x").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "ab",
            "空选区剪切文本应不变"
        );
        assert_eq!(
            wcx.read_from_clipboard().and_then(|i| i.text()),
            Some("KEEP".to_string()),
            "空选区剪切不应改写剪贴板"
        );
        assert_eq!(changes.borrow().0, before, "空选区剪切不应发 Change");
    }

    /// R3：连续内嵌输入（字符，间隔 <300ms、无光标移动）合并为一个撤销单元——一次
    /// Ctrl+Z 撤掉整段 "abc"，Ctrl+Y 回推。
    #[gpui::test]
    async fn continuous_typing_merges_into_one_undo_group(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "a", wcx);
        type_text(&input, "b", wcx);
        type_text(&input, "c", wcx);
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "abc"
        );
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "连续输入应合并为一个撤销单元，一次撤销回空"
        );
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-y").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "abc",
            "重做应回推整段连续输入"
        );
    }

    /// R3：超过 300ms 停顿（时间窗）→ 新起撤销单元：两次输入各成一组，两次撤销逐段回退。
    #[gpui::test]
    async fn time_break_splits_undo_group(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "a", wcx);
        // 伪造时间窗过期：把开放组上次编辑时刻拨回 400ms 前（> UNDO_GROUP_MS=300）。
        wcx.update(|_, cx| {
            input.update(cx, |s, _| {
                if let Some(o) = s.undo_open.as_mut() {
                    o.last_edit_at = Instant::now() - Duration::from_millis(400);
                }
            });
        });
        type_text(&input, "b", wcx);
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "ab");
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "a",
            "停顿 ≥300ms 应新起撤销单元，第一次撤销只回退 b"
        );
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "第二次撤销回退 a"
        );
    }

    /// R3：光标移动断层撤销合并——"ab" 后移光标到行首再输入 "X"：第一次撤销只回退 X、
    /// 第二次才回退 ab。
    #[gpui::test]
    async fn cursor_move_breaks_merge_and_input_lands_new_group(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "ab", wcx); // 合并为一组
        let w = focus_input_for_keys(&input, wcx);
        // 光标移动（Ctrl+← 到行首）→ 断层。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-left").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).selection.clone()),
            0..0,
            "Ctrl+← 应把光标移到行首"
        );
        type_text(&input, "X", wcx); // 行首插入
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "Xab");
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "ab",
            "光标移动后新输入应独立成组，撤销只回退 X"
        );
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "再撤销回退整段 ab（其自身一组）"
        );
    }

    /// R3+R1：粘贴永不与前后内嵌输入合并——输入 "a"（一组）后粘贴 "z"（独一组）：
    /// 第一次撤销回 "a"、第二次回 ""。
    #[gpui::test]
    async fn paste_is_separate_undo_group(cx: &mut TestAppContext) {
        bind_paste_normalized(cx);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "a", wcx);
        wcx.write_to_clipboard(gpui::ClipboardItem::new_string("z".to_string()));
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-v").unwrap());
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "az");
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "a",
            "粘贴应独立成组，第一次撤销只回退粘贴 z"
        );
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "第二次撤销回退输入 a"
        );
    }

    /// R3：重做分叉作废——撤销后新编辑 → Ctrl+Y 无动作（旧重做分支整棵作废）。
    #[gpui::test]
    async fn redo_branch_invalidated_by_new_edit(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "ab", wcx);
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "");
        // 撤销后新输入 → 重做分支作废。
        type_text(&input, "x", wcx);
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "x");
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-y").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "x",
            "撤销后新编辑应作废重做分支，Ctrl+Y 无动作"
        );
        // 但撤销仍可用：回退新输入 x。
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "新输入 x 仍可单独撤销"
        );
    }

    /// R3：撤销栈深度上限 [`UNDO_LIMIT`]=200——210 次 Atomic（粘贴）各成一单元，栈被
    /// 压到上限、丢最旧；文本全部落地（210 个 x）。
    #[gpui::test]
    async fn undo_stack_respects_limit(cx: &mut TestAppContext) {
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        wcx.update(|_window, cx| {
            input.update(cx, |s, cx| {
                for _ in 0..210 {
                    s.do_paste("x", cx);
                }
            });
        });
        let len = wcx.update(|_, cx| input.read(cx).undo_stack.len());
        assert_eq!(
            len,
            UNDO_LIMIT,
            "撤销栈应被压到上限 {}（丢最旧），实际 {len}",
            UNDO_LIMIT
        );
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "x".repeat(210),
            "210 次粘贴都落地（文本 = 210 个 x）"
        );
    }

    /// R3：Enter 提交封口本单元——"ab" 输入后 Enter（Submit 计数+1），再输入 "c"：
    /// 第一次撤销只回退 c，第二次才回退整段 ab。
    #[gpui::test]
    async fn enter_submit_seals_undo_group(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, events) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "ab", wcx);
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("enter").unwrap());
        assert_eq!(events.borrow().1, 1, "Enter 应发一次 Submit");
        type_text(&input, "c", wcx); // 封口后再输入 → 独立成组
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "abc");
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "ab",
            "第一次撤销只回退 Enter 后输入 c"
        );
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "",
            "第二次撤销回退已封口的 ab 组"
        );
    }

    /// R3：整体替换（`set_value`）清空撤销历史——替换后 Ctrl+Z 无动作。
    #[gpui::test]
    async fn set_value_clears_undo_history(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "");
        settle_window(wcx);
        type_text(&input, "ab", wcx);
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| s.set_value("xy", window, cx));
        });
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "xy");
        let (u, r) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.undo_stack.len(), s.redo_stack.len())
        });
        assert_eq!((u, r), (0, 0), "set_value 应清空撤销/重做栈");
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "xy",
            "set_value 清空历史后 Ctrl+Z 无动作"
        );
    }

    /// R3：IME 组合提交独立一组——组合更新不产生撤销单元（不逐键记栈），提交整体作为
    /// 一次撤销：ab → 组合 "中" → 提交 → ab中；一次 Ctrl+Z 回到 ab。
    #[gpui::test]
    async fn ime_commit_is_single_undo_group(cx: &mut TestAppContext) {
        cx.update(crate::test_support::bind_local_keymaps);
        let (input, wcx, _) = add_input(cx, "ab");
        settle_window(wcx);
        // Windows IME：组合更新（None → 替换 marked/选区）→ 提交（None，Typing 意图）。
        wcx.update(|window, cx| {
            input.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(None, "中", None, window, cx);
                s.replace_text_in_range(None, "中", window, cx);
            });
        });
        assert_eq!(wcx.update(|_, cx| input.read(cx).value().to_string()), "ab中");
        // 组合更新不应在栈里留下中间态：一份撤销 = 移除整个组合。
        let (u, r) = wcx.update(|_, cx| {
            let s = input.read(cx);
            (s.undo_stack.len(), s.redo_stack.len())
        });
        assert_eq!((u, r), (1, 0), "组合提交应是一个独立撤销单元");
        let w = focus_input_for_keys(&input, wcx);
        wcx.dispatch_keystroke(w, gpui::Keystroke::parse("ctrl-z").unwrap());
        assert_eq!(
            wcx.update(|_, cx| input.read(cx).value().to_string()),
            "ab",
            "一次 Ctrl+Z 应整体撤销 IME 组合提交"
        );
    }
}
