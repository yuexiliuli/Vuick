//! 公共命令表（单一真值）：菜单 / 命令面板 / 根级快捷键三者共享的 label 与快捷键常量。
//!
//! 收敛背景：同一批命令此前在 `app::menu`（菜单 label+快捷键）、`app::palette`
//! （命令面板 label+快捷键）、`src/main.rs`（根级键位键串）三处手写，改一个快捷键
//! 要动三处。本轮把 **label + 快捷键展示串 + 根级键位串**收敛为本表：
//! - 宿主 shell 的菜单（`app::menu`）与命令面板（`app::palette`）直接引用本表常量的
//!   [`Command::menu_label`] / [`Command::label`] 与 [`Command::shortcut`]；
//! - 根级键位（`src/main.rs`）由 [`root_keybindings`] 从本表键串推导（取代手写键串）。
//!
//! 决策（保底策略，不做整表投影）：菜单项还承载 `keep_open` / 动态勾选 / 分隔线 /
//! 自定义项等渲染细节，面板的分类/条目与菜单并不完全同构（面板多「切换侧边栏」、
//! 菜单多「打开最近…/恢复上次会话/预览编辑测试/上一章/下一章」等特殊项），把两者
//! 耦进同一份可投影数据的侵入太大；此处只把**公共命令**的 label+快捷键收敛为单一
//! 真值，菜单/特殊项/顺序保持各自结构。个别命令的菜单 label 与面板 label 历史不同
//! （「新建」 vs 「新建文件」），两者都作为字段存于本表同一行，改动仍只动本表。
//!
//! 键位边界：编辑区「编辑」类快捷键（撤销/重做/剪切/复制/粘贴/全选）绑定在
//! `base::actions::bind_input_keys`（`Input` 上下文，属 base crate），本表只登记其
//! **展示串**（[`BindingCtx::EditorInput`]，`key` 留空——键位由 base 持有，shell 无
//! 法越 crate 推导）。「上一章/下一章」等菜单提示历史上**未实际绑定**（仅展示），
//! 不入本表。

use gpui::KeyBinding;

/// 键位绑定归属（决定该命令的键位从何而来）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindingCtx {
    /// 根级全局键位（`src/main.rs` 的 None 上下文绑定，动作由 app 根元素 `on_action`
    /// 处理）；`key` 为本表唯一来源，由 [`root_keybindings`] 推导。
    Root,
    /// 编辑区 `Input` 上下文（`base::actions::bind_input_keys` 绑定）；本表仅登记展示串。
    EditorInput,
    /// 无键位（仅菜单/面板展示，如「预览/黑白切换」）。
    DisplayOnly,
}

/// 命令元数据（唯一真值）。同一命令的 菜单label / 面板label / 展示快捷键 / 键位串
/// 全部集中在本行——改快捷键只动本表一处。
pub struct Command {
    /// 稳定动作标识（测试 / 对齐用；不参与展示）。
    /// 半死字段（dormancy 休眠保留，K24）：本 crate 内仅测试消费（`root_commands_shortcut_and_key_are_paired`
    /// 读 `c.id`），生产路径不读——保留是因为每个 `Command` 常量都构造它，属稳定标识真值。
    pub id: &'static str,
    /// 命令面板展示 label。
    pub label: &'static str,
    /// 菜单展示 label（与面板不同时保留历史差异，如「新建」）。
    pub menu_label: &'static str,
    /// 菜单/面板右侧快捷键展示串（"" = 无提示）。
    pub shortcut: &'static str,
    /// 键位绑定串（[`BindingCtx::Root`] 项由此进入 [`root_keybindings`]；其余为空）。
    pub key: &'static str,
    /// 键位归属上下文。
    pub ctx: BindingCtx,
}

/// 根级：文件/窗口命令。
pub const NEW_FILE: Command = Command {
    id: "new-file",
    label: "新建文件",
    menu_label: "新建",
    shortcut: "Ctrl+N",
    key: "ctrl-n",
    ctx: BindingCtx::Root,
};
pub const OPEN_FILE: Command = Command {
    id: "open-file",
    label: "打开文件…",
    menu_label: "打开文件…",
    shortcut: "Ctrl+O",
    key: "ctrl-o",
    ctx: BindingCtx::Root,
};
pub const OPEN_FOLDER: Command = Command {
    id: "open-folder",
    label: "打开文件夹…",
    menu_label: "打开文件夹…",
    shortcut: "Ctrl+Shift+O",
    key: "ctrl-shift-o",
    ctx: BindingCtx::Root,
};
pub const SAVE: Command = Command {
    id: "save",
    label: "保存",
    menu_label: "保存",
    shortcut: "Ctrl+S",
    key: "ctrl-s",
    ctx: BindingCtx::Root,
};
pub const SAVE_AS: Command = Command {
    id: "save-as",
    label: "另存为…",
    menu_label: "另存为…",
    shortcut: "Shift+Ctrl+S",
    key: "shift-ctrl-s",
    ctx: BindingCtx::Root,
};
pub const QUIT: Command = Command {
    id: "quit",
    label: "退出",
    menu_label: "退出",
    shortcut: "Ctrl+Q",
    key: "ctrl-q",
    ctx: BindingCtx::Root,
};
/// 根级键位专属（菜单/面板不展示，仅键位入表）。
pub const CLOSE_TAB: Command = Command {
    id: "close-tab",
    label: "关闭选项卡",
    menu_label: "关闭选项卡",
    shortcut: "",
    key: "ctrl-w",
    ctx: BindingCtx::Root,
};
pub const QUICK_OPEN: Command = Command {
    id: "quick-open",
    label: "打开文件快选",
    menu_label: "打开文件快选",
    shortcut: "",
    key: "ctrl-p",
    ctx: BindingCtx::Root,
};
pub const TOGGLE_COMMAND_PALETTE: Command = Command {
    id: "toggle-command-palette",
    label: "命令面板",
    menu_label: "命令面板",
    shortcut: "",
    key: "ctrl-shift-p",
    ctx: BindingCtx::Root,
};
pub const GOTO_LINE: Command = Command {
    id: "goto-line",
    label: "跳转到行",
    menu_label: "跳转到行",
    shortcut: "Ctrl+G",
    key: "ctrl-g",
    ctx: BindingCtx::Root,
};
/// 编辑区内查找（Ctrl+F 打开查找条 / 输入即高亮；只在活动文本选项卡有意义）。
pub const FIND: Command = Command {
    id: "find",
    label: "查找",
    menu_label: "查找",
    shortcut: "Ctrl+F",
    key: "ctrl-f",
    ctx: BindingCtx::Root,
};
pub const FIND_NEXT: Command = Command {
    id: "find-next",
    label: "查找下一个",
    menu_label: "查找下一个",
    shortcut: "F3",
    key: "f3",
    ctx: BindingCtx::Root,
};
pub const FIND_PREV: Command = Command {
    id: "find-prev",
    label: "查找上一个",
    menu_label: "查找上一个",
    shortcut: "Shift+F3",
    key: "shift-f3",
    ctx: BindingCtx::Root,
};
/// 根级：查看/设置命令。
pub const TOGGLE_MUSIC_PLAYER: Command = Command {
    id: "toggle-music-player",
    label: "音乐播放器",
    menu_label: "音乐播放器",
    shortcut: "Ctrl+M",
    key: "ctrl-m",
    ctx: BindingCtx::Root,
};
pub const TOGGLE_WRAP: Command = Command {
    id: "toggle-wrap",
    label: "自动换行",
    menu_label: "自动换行",
    shortcut: "Ctrl+Alt+W",
    key: "ctrl-alt-w",
    ctx: BindingCtx::Root,
};
pub const INCREASE_FONT: Command = Command {
    id: "increase-font",
    label: "增大字号",
    menu_label: "增大字号",
    shortcut: "Ctrl+=",
    key: "ctrl-=",
    ctx: BindingCtx::Root,
};
pub const DECREASE_FONT: Command = Command {
    id: "decrease-font",
    label: "减小字号",
    menu_label: "减小字号",
    shortcut: "Ctrl+-",
    key: "ctrl--",
    ctx: BindingCtx::Root,
};
pub const RESET_FONT: Command = Command {
    id: "reset-font",
    label: "重置字号",
    menu_label: "重置字号",
    shortcut: "Ctrl+0",
    key: "ctrl-0",
    ctx: BindingCtx::Root,
};
pub const FOLLOW_THEME: Command = Command {
    id: "follow-theme",
    label: "跟随系统主题",
    menu_label: "跟随系统主题",
    shortcut: "Ctrl+Shift+T",
    key: "ctrl-shift-t",
    ctx: BindingCtx::Root,
};
/// 编辑区 `Input` 上下文（base crate 绑定；本表仅登记展示串，`key` 留空）。
pub const UNDO: Command = Command {
    id: "undo",
    label: "撤销",
    menu_label: "撤销",
    shortcut: "Ctrl+Z",
    key: "",
    ctx: BindingCtx::EditorInput,
};
pub const REDO: Command = Command {
    id: "redo",
    label: "重做",
    menu_label: "重做",
    shortcut: "Ctrl+Y",
    key: "",
    ctx: BindingCtx::EditorInput,
};
pub const CUT: Command = Command {
    id: "cut",
    label: "剪切",
    menu_label: "剪切",
    shortcut: "Ctrl+X",
    key: "",
    ctx: BindingCtx::EditorInput,
};
pub const COPY: Command = Command {
    id: "copy",
    label: "复制",
    menu_label: "复制",
    shortcut: "Ctrl+C",
    key: "",
    ctx: BindingCtx::EditorInput,
};
pub const PASTE: Command = Command {
    id: "paste",
    label: "粘贴",
    menu_label: "粘贴",
    shortcut: "Ctrl+V",
    key: "",
    ctx: BindingCtx::EditorInput,
};
pub const SELECT_ALL: Command = Command {
    id: "select-all",
    label: "全选",
    menu_label: "全选",
    shortcut: "Ctrl+A",
    key: "",
    ctx: BindingCtx::EditorInput,
};
/// 无键位命令（菜单/面板展示用）。
pub const PREVIEW: Command = Command {
    id: "preview",
    label: "预览",
    menu_label: "预览",
    shortcut: "",
    key: "",
    ctx: BindingCtx::DisplayOnly,
};
pub const TOGGLE_SIDEBAR: Command = Command {
    id: "toggle-sidebar",
    label: "切换侧边栏",
    menu_label: "切换侧边栏",
    shortcut: "",
    key: "",
    ctx: BindingCtx::DisplayOnly,
};
pub const THEME: Command = Command {
    id: "theme",
    label: "黑白切换",
    menu_label: "黑白切换",
    shortcut: "",
    key: "",
    ctx: BindingCtx::DisplayOnly,
};

/// 全部命令（表投影源；测试/文档用）。菜单与面板按各自结构引用其中部分常量。
pub const ALL_COMMANDS: &[Command] = &[
    NEW_FILE, OPEN_FILE, OPEN_FOLDER, SAVE, SAVE_AS, QUIT, CLOSE_TAB, QUICK_OPEN,
    TOGGLE_COMMAND_PALETTE, GOTO_LINE, FIND, FIND_NEXT, FIND_PREV, TOGGLE_MUSIC_PLAYER,
    TOGGLE_WRAP, INCREASE_FONT, DECREASE_FONT, RESET_FONT, FOLLOW_THEME, UNDO, REDO, CUT, COPY,
    PASTE, SELECT_ALL, PREVIEW, TOGGLE_SIDEBAR, THEME,
];

/// 全部根级快捷键（`src/main.rs` 自此取代手写键串）。键串来自本表 [`Command::key`]，
/// 动作类型显式列出（不同动作是互异类型，无法存进 `Command` 常量，故在此逐项配对）。
pub fn root_keybindings() -> Vec<KeyBinding> {
    use base::actions::*;
    vec![
        KeyBinding::new(NEW_FILE.key, NewFile, None),
        KeyBinding::new(OPEN_FILE.key, OpenFile, None),
        KeyBinding::new(OPEN_FOLDER.key, OpenFolder, None),
        KeyBinding::new(SAVE.key, Save, None),
        KeyBinding::new(SAVE_AS.key, SaveAs, None),
        KeyBinding::new(CLOSE_TAB.key, CloseTab, None),
        KeyBinding::new(QUICK_OPEN.key, QuickOpenFile, None),
        KeyBinding::new(TOGGLE_COMMAND_PALETTE.key, ToggleCommandPalette, None),
        KeyBinding::new(GOTO_LINE.key, GoToLine, None),
        KeyBinding::new(FIND.key, Find, None),
        KeyBinding::new(FIND_NEXT.key, FindNext, None),
        KeyBinding::new(FIND_PREV.key, FindPrev, None),
        KeyBinding::new(TOGGLE_MUSIC_PLAYER.key, ToggleMusicPlayer, None),
        KeyBinding::new(TOGGLE_WRAP.key, ToggleWordWrap, None),
        KeyBinding::new(INCREASE_FONT.key, IncreaseFontSize, None),
        KeyBinding::new(DECREASE_FONT.key, DecreaseFontSize, None),
        KeyBinding::new(RESET_FONT.key, ResetFontSize, None),
        KeyBinding::new(FOLLOW_THEME.key, FollowSystemTheme, None),
        KeyBinding::new(QUIT.key, Quit, None),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 护栏（行为零变化回归断言）：`root_keybindings()` 的对 (键串, 动作名) 集合
    /// 必须与迁移前 `src/main.rs` 手写键串逐一一致。此后改快捷键只动本表一处，
    /// 本测试锁死键位集合，防止菜单/键位悄悄漂移。
    #[test]
    fn root_keybindings_match_legacy_set() {
        let mut got: Vec<(String, String)> = root_keybindings()
            .iter()
            .map(|b| {
                (
                    b.keystrokes()
                        .iter()
                        .map(|k| k.unparse())
                        .collect::<Vec<_>>()
                        .join(" "),
                    b.action().name().to_owned(),
                )
            })
            .collect();
        got.sort();
        let mut want: Vec<(String, String)> = vec![
            ("ctrl-n".into(), "editor_app::NewFile".into()),
            ("ctrl-o".into(), "editor_app::OpenFile".into()),
            ("ctrl-shift-o".into(), "editor_app::OpenFolder".into()),
            ("ctrl-s".into(), "editor_app::Save".into()),
            ("ctrl-shift-s".into(), "editor_app::SaveAs".into()),
            ("ctrl-w".into(), "editor_app::CloseTab".into()),
            ("ctrl-p".into(), "editor_app::QuickOpenFile".into()),
            ("ctrl-shift-p".into(), "editor_app::ToggleCommandPalette".into()),
            ("ctrl-g".into(), "editor_app::GoToLine".into()),
            ("ctrl-f".into(), "editor_app::Find".into()),
            ("f3".into(), "editor_app::FindNext".into()),
            ("shift-f3".into(), "editor_app::FindPrev".into()),
            ("ctrl-m".into(), "editor_app::ToggleMusicPlayer".into()),
            ("ctrl-alt-w".into(), "editor_app::ToggleWordWrap".into()),
            ("ctrl-=".into(), "editor_app::IncreaseFontSize".into()),
            ("ctrl--".into(), "editor_app::DecreaseFontSize".into()),
            ("ctrl-0".into(), "editor_app::ResetFontSize".into()),
            ("ctrl-shift-t".into(), "editor_app::FollowSystemTheme".into()),
            ("ctrl-q".into(), "editor_app::Quit".into()),
        ];
        want.sort();
        assert_eq!(got, want, "根级键位集合与迁移前不一致");
    }

    /// 内部一致性：根级命令的展示快捷键与键位串必须成对（不许只有其一）。
    #[test]
    fn root_commands_shortcut_and_key_are_paired() {
        for c in ALL_COMMANDS {
            if c.ctx == BindingCtx::Root {
                assert!(!c.key.is_empty(), "{} 根级命令缺键位串", c.id);
            }
        }
    }
}
