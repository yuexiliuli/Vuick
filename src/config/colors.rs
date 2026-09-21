//! 主题色板常量：统一存 `u32` hex，使用处 `rgb(x).into()` 转 `Hsla`。

/// 全局统一强调色（可拖拽分隔条/分割线、激活选项卡顶强调线、目录强调点、
/// 侧边栏选中项等共用，改这一处即可统一全部）。
pub const RESIZE_ACCENT: u32 = 0x0078d4;

/// 链接蓝（明/暗）——预览 / WYSIWYG / 源码编辑器三处链接统一取这里的值
/// （GitHub 蓝）。改这一处，三处链接色一起变。
pub const LINK_LIGHT: u32 = 0x0969da;
pub const LINK_DARK: u32 = 0x79c0ff;
