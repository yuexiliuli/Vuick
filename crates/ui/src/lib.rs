//! 自研 UI 组件库：逐步替代 `gpui-component`。
//!
//! # 目标
//! 本项目原依赖 `gpui-component` 的组件（Theme/Root/Scrollbar/Icon/Button/Menu/
//! Notification/Input…），本 crate 承载**自定义实现**，按组件逐个替换、渐进移除依赖。
//! 见 `docs/ui-migration.md` 的依赖台账与顺序。
//!
//! # 目录结构
//! - [`theme`]：自研主题（ColorScheme/ThemeMode/Theme global），替换 `gpui_component::Theme`
//! - [`icon`]：自绘图标（`IconName` + `Icon`），替换 `gpui_component::{Icon, IconName}`
//! - [`scrollbar`]：滚动条（`Scrollbar`/`ScrollbarHandle`/`ScrollbarMode`），替换 `gpui_component::scroll`
//! - [`button`]：按钮（`Button`/`ButtonVariant`/`ButtonVariants`），替换 `gpui_component::button`
//! - [`empty_state`]：空状态占位（`empty_illustration`/`empty_state_hero`/`compact_empty_state`），
//!   无标签页 / 空目录 / 无搜索命中的自绘插图 + 文案
//! - [`menu`]：下拉菜单（`PopupMenu`/`PopupMenuItem`/`Dropdown`/`DropdownMenu`），替换 `gpui_component::menu`
//! - [`notification`]：通知（`Notification`/`NotificationKind`），替换 `gpui_component::notification`
//! - [`palette`]：命令面板（`CommandPalette`/`PaletteMode`/`PaletteItem`），居中浮层，命令 + 文件快选两用
//! - [`root`]：窗口根视图（每窗口第一层宿主），替换 `gpui_component::Root`
//! - [`input`]：单行文本输入框（搜索框），替换 `gpui_component::input::Input`/`InputState`
//! - [`shell`]：外壳卡片化共享几何（卡间距/圆角）与「三点分隔手柄」（splitter/resize 复用）
//!
//! # 组件约定（新组件照此执行）
//! 1. **一文件一组件**，文件放在本目录（或独立子目录），公开面收敛到 `ui::<模块>`，
//!    使用处 `use ui::xxx`（组件多时经本 `lib.rs` 模块声明走 `ui::` 命名空间）。
//! 2. **样式单一来源**：颜色一律取 `theme::ColorScheme` 语义字段，禁止新写硬编码 `rgba()`；
//!    现有散落的字面量色值在迁移时收编进对应语义字段，原常量可保留兼容 re-export。
//! 3. **跨帧交互状态**用 `window` 的元素级状态（`use_keyed_state`/元素 id）或
//!    调用方持有的 `Rc<RefCell<..>>` 保持（参考 `scrollbar` 的 `TableScrollState` 前世）。
//!    关键：按「元素实际 id」隔离状态，避免 `use_state` 的调用点定位导致多实例串扰。
//! 4. **每个组件配 `gpui::test`**：至少覆盖「可见/隐藏条件」与「交互写回」两条路径。
//! 5. 风格：edition 2024、尾表达式、clippy 零告警（与仓库现行纪律一致）。
//! 6. **无新增依赖**：能用 gpui 原生能力（`Path`/`paint_svg`/`paint_quad`/`ScrollHandle`）
//!    就不引第三方；gpui 自带的能力（如 SVG 光栅）视为零成本。
//!
//! # 迁移纪律
//! - 每个组件做完必须连同「调用点迁移 + 测试 + 视觉无回归」一起提交，不留下半截状态。
//! - 依赖已移除：不再有 `gpui_component` 兼容桥（`Theme::change` 等）。
//!
//! # 依赖（P3a 迁出后断环声明）
//! 本 crate 依赖集 = `{config, base, ime}` + gpui/regex，**不再依赖 editor**
//! （原 `ui/input.rs` 对 `editor::EditIntent` 的唯一引用已改走 `base::EditIntent`）。
//! 以下 crate 根短别名让迁移期 `config::` / `crate::actions::` / `ime::` /
//! `crate::ui::` 旧路径零改动即可解析（与根包 P1/P2 的别名过渡同一策略）。

pub mod button;
pub mod empty_state;
pub mod icon;
pub mod input;
pub mod menu;
pub mod motion;
pub mod notification;
pub mod palette;
pub mod root;
pub mod scrollbar;
pub mod shell;
pub mod theme;
pub mod tooltip;

/// 迁移期别名：让本 crate 内部既有的 `crate::ui::` 路径（自引用）保持解析。
use crate as ui;
/// 迁移期别名：`crate::actions::` → `base::actions`（动作类型已下沉 base）。
use base::actions;

/// 测试键位助手：`ui` 独立 crate 后不能再调根包 `crate::actions::bind_local_keymaps`
/// （该组合含 markdown 部分且定义在根包），本转向 `base::bind_input_keys +
/// `crate::root::bind_keys`（即原组合去掉 markdown 后的片段，等价原 Input+Root 两套
/// 键上下文）。
#[cfg(test)]
pub mod test_support;
