//! 应用级动作类型（与 gpui-component 内建输入动作区分）。
//!
//! 文本编辑的快捷键（backspace/ctrl-v/undo…）由 gpui-component 的 `Input` 控件
//! 在 `Input` 键上下文中自行处理；这里只注册**文件/窗口应用层面**的动作。

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
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        Quit,
    ]
);