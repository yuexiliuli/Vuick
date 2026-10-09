//! 应用级动作类型。
//!
//! 分两层：
//! - `editor_app`：文件/窗口应用层面的动作（NewFile/OpenFile/Save/…）。
//! - `editor_input`：编辑区文本编辑动作（MoveLeft/Undo/Backspace/…）。这些是
//!   `gpui_component::input` 同名 action 的**本地化副本**——由
//!   [`bind_input_keys`] 在 `Input` 键上下文自绑快捷键（替代组件库在
//!   `gpui_component::init` 里的绑定），编辑器/菜单/测试统一切到本地类型。

use gpui::actions;

actions!(
    editor_app,
    [
        NewFile,
        OpenFile,
        Save,
        SaveAs,
        CloseTab,
        TogglePreview,
        ToggleWordWrap,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        FollowSystemTheme,
        PasteNormalized,
        // P2-8 命令面板：Ctrl+Shift+P 打开命令面板、Ctrl+P 打开文件快选。
        ToggleCommandPalette,
        QuickOpenFile,
        Quit,
    ]
);

actions!(
    editor_input,
    [
        Backspace,
        Copy,
        Cut,
        Delete,
        DeleteToNextWordEnd,
        DeleteToPreviousWordStart,
        Enter,
        IndentInline,
        MoveDown,
        MoveEnd,
        MoveHome,
        MoveLeft,
        MovePageDown,
        MovePageUp,
        MoveRight,
        MoveToEnd,
        MoveToNextWord,
        MoveToPreviousWord,
        MoveToStart,
        MoveUp,
        OutdentInline,
        Paste,
        Redo,
        SelectAll,
        SelectToEnd,
        SelectToEndOfLine,
        SelectToNextWordEnd,
        SelectToPreviousWordStart,
        SelectToStart,
        SelectToStartOfLine,
        Undo,
    ]
);

/// 把编辑区输入快捷键绑定到 `editor_input` 本地 action（`Input` 键上下文）。
///
/// 这是 `gpui_component::init` 里 `Input` 上下文 keymap 的**本地复制**（Windows
/// 子集：macOS 的 cmd-* 变体不迁），只是绑定目标换成自研编辑器 action 类型；
/// 否则编辑器从组件 action 换到本地 action 后快捷键会全失。
///
/// 注册顺序即优先级，必须在二者之间调用：
/// 1. **在 `gpui_component::init` 之后**——本地绑定优先级更高，编辑器聚焦时
///    先命中本地 action（其 handler 会 `stop_propagation`，组件库同名绑定不再触发）；
/// 2. **在 `PasteNormalized` 绑定（`main.rs` 的 ctrl-v 覆盖）之前**——Ctrl+V 继续
///    只走归一化粘贴（优先级更高且被处理即停传播），`Paste` 不在此处绑定。
///
/// 搜索框（真实 `gpui_component::Input`）持有时本地 action 无处理者，会自然落到
/// 组件库绑定，二者互不影响。
pub fn bind_input_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    const CONTEXT: &str = "Input";
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("shift-backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("shift-delete", Delete, Some(CONTEXT)),
        KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("shift-enter", Enter, Some(CONTEXT)),
        KeyBinding::new("secondary-enter", Enter, Some(CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(CONTEXT)),
        KeyBinding::new("pageup", MovePageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", MovePageDown, Some(CONTEXT)),
        KeyBinding::new("tab", IndentInline, Some(CONTEXT)),
        KeyBinding::new("shift-tab", OutdentInline, Some(CONTEXT)),
        KeyBinding::new("home", MoveHome, Some(CONTEXT)),
        KeyBinding::new("end", MoveEnd, Some(CONTEXT)),
        KeyBinding::new("shift-home", SelectToStartOfLine, Some(CONTEXT)),
        KeyBinding::new("shift-end", SelectToEndOfLine, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(CONTEXT)),
        KeyBinding::new("ctrl-right", MoveToNextWord, Some(CONTEXT)),
    ])
}

/// 安装全部本地快捷键（编辑区 Input + 窗口根 Root + WYSIWYG wy-editor 三种键上下文）。
///
/// 替代已移除的 `gpui_component::init` 的按键映射职责，供 `main.rs` 与测试初始化共用。
/// 调用方须在随后注册比 `Input` 上下文更高优先级的覆盖绑定（如 `ctrl-v` →
/// `PasteNormalized`），顺序＝优先级。
pub fn bind_local_keymaps(cx: &mut gpui::App) {
    bind_input_keys(cx);
    crate::ui::root::bind_keys(cx);
    crate::markdown::wysiwyg::bind_keys(cx);
}
