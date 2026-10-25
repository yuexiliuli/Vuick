//! 自研主题系统：本应用 UI 配色/明暗的唯一来源（`gpui_component::Theme` 已随依赖移除）。
//!
//! # 获取
//! - [`Theme::global(cx)`] / [`Theme::global_mut(cx)`]：未初始化时回落到内置 Light 主题
//!   （`try_global` 兜底——测试里不显式初始化也能零成本使用）。
//! - 启动时调用 [`Theme::init(cx)`] 显式种入默认主题；切换走 [`Theme::set`]。
//!
//! # 语义色板
//! 颜色统一从这里取语义字段，禁止在组件里新写硬编码 `rgba()`。第 3 波迁移后，
//! Markdown 预览 / WYSIWYG / 编辑器的排版与配色也已接入本主题（`markdown/view/style.rs`、
//! `editor/view` 的 typography、`editor/markdown.rs`、`syntax` 深耦合点全部收编，
//! 见 `docs/ui-migration.md`）。残留的 gpui-component 消费还剩编辑器 Input 本体（#12）。

use std::sync::LazyLock;

use gpui::{App, Global, Hsla, Rgba, SharedString, hsla, rgb, rgba};

/// 明暗模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// 排版参数（与色板同属主题、跨明暗的字体家族与字号体系）。
///
/// 「正文/标题字号体系」由 `config::layout::DEFAULT_FONT_SIZE` 驱动，本结构固化缺省
/// 字号与缩放比例，编辑器/预览/WYSIWYG 各自从这里读默认值：
/// `editor_default_size` / `editor_line_height_ratio` 是编辑器的等宽缺省字号与行高
/// 系数（无 `font_override` 时用；保留 gpui-component `mono_md` 的 13/20 既有值，
/// 视觉无回归——应用侧恒有 override=14，此值仅供无覆盖/测试路径）；
/// `default_body_size` 是预览 / WYSIWYG / 编辑区覆盖路径的统一缩放基准（与
/// `config::layout::DEFAULT_FONT_SIZE` 同值）。正文行高/代码字号/标题字号的推导
/// 函数集中在 `markdown::view::style::{typography_for_size, heading_metrics_for_size}`，
/// 即统一缩放架构（见 `docs/ui-migration.md`）。
#[derive(Clone, Debug)]
pub struct Typography {
    /// 等宽字体族（代码块/编辑器/行内代码/语法高亮共用；平台默认，Windows=Consolas）。
    pub mono: SharedString,
    /// 编辑器等宽缺省字号（px，`font_override=None` 时用；原 `mono_md` 默认 13）。
    pub editor_default_size: f32,
    /// 编辑器等宽行高系数（× 字号）；原 `mono_md` 20/13 比例。
    pub editor_line_height_ratio: f32,
    /// 缺省正文字号（px，预览/WYSIWYG 默认与编辑区覆盖路径的统一缩放基准，
    /// 与 `config::layout::DEFAULT_FONT_SIZE` 同值）。
    pub default_body_size: f32,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            mono: default_mono_font_family(),
            editor_default_size: 13.0,
            editor_line_height_ratio: 20.0 / 13.0,
            default_body_size: config::layout::DEFAULT_FONT_SIZE,
        }
    }
}

/// 平台默认等宽字体（Windows=`Consolas`，macOS=`Menlo`，其余=`DejaVu Sans Mono`），
/// 与 gpui-component 默认一致，避免换主题后代码字形跳变。
fn default_mono_font_family() -> SharedString {
    if cfg!(target_os = "macos") {
        "Menlo".into()
    } else if cfg!(target_os = "windows") {
        "Consolas".into()
    } else {
        "DejaVu Sans Mono".into()
    }
}

/// 语义色板（亮/暗各一套）。字段名是「用途」而非具体色值。
///
/// 第 3 波迁移后已全量消费：`bg`/`surface`/`fg`/`fg_muted`/`border`/`accent`/`link`
/// 供通用控件；`code_*`/`quote_bg`/`table_head_bg`/`heading`/`popover_bg` 供
/// Markdown 预览 / WYSIWYG / 编辑器（原 `markdown/view/style.rs`、`note_card`、
/// `editor/markdown.rs` 的硬编码 `rgb()` 收编于此，见 `docs/ui-migration.md`）。
/// P1-7 起又收编状态语义色：`fg_disabled`（禁用）、`focus_ring`（聚焦描边）、
/// `selected_bg`/`pressed_bg`（选中/按压基底）、`drag_hover_border`（拖拽强调线）。
#[derive(Clone, Copy, Debug)]
pub struct ColorScheme {
    /// 主背景（画布/正文底色）
    pub bg: Rgba,
    /// 面板/状态栏等次级表面
    pub surface: Rgba,
    /// 主文字
    pub fg: Rgba,
    /// 次要文字
    pub fg_muted: Rgba,
    /// 分隔线/边框
    pub border: Rgba,
    /// 强调色（拖拽分栏/激活高亮/引用标记共用，原 `config::colors` 的 RESIZE_ACCENT）
    pub accent: Rgba,
    /// 链接（原 `config::colors` 的 LINK_LIGHT/DARK）
    pub link: Rgba,
    /// 错误/危险色（打开失败通知等）
    pub error: Rgba,
    /// 禁用态弱化文字（菜单项等置灰，明浅灰 / 暗深灰，比 fg_muted 更弱一档）
    pub fg_disabled: Rgba,
    /// 聚焦控件描边（侧栏搜索框 / 命令面板查询框等 Fluent 聚焦框，亮暗 = 对应档强调色）
    pub focus_ring: Rgba,
    /// 选中/悬停基底（按钮等控件 hover/selected 着染，fg 低透明：亮 ~8% / 暗 ~14%）
    pub selected_bg: Rgba,
    /// 按压/激活基底（比 selected_bg 深一档：亮 ~14% / 暗 ~24%）
    pub pressed_bg: Rgba,
    /// 拖拽/悬停强调线（分栏 splitter、侧栏右缘调宽等，亮暗 = 对应档强调色）
    pub drag_hover_border: Rgba,
    /// 代码块背景（预览 / WYSIWYG 围栏底，明 #f5f5f5 / 暗 #262626）
    pub code_block_bg: Rgba,
    /// 行内代码（反引号）文字色（明 #202020 / 暗 #e8e8e8，近正文，靠底色+等宽区分）
    pub code_inline_fg: Rgba,
    /// 行内代码圆角 pill 底色（预览 / WYSIWYG，明 #f0f0f0 / 暗 #3a3a3a）
    pub code_inline_bg: Rgba,
    /// 编辑器 Markdown 滤镜行内代码底色（明 #f2f2f2 / 暗 #2e2e2e，与预览略异）
    pub editor_code_bg: Rgba,
    /// 公司块背景（明 #f7f7f7 / 暗 #262626）
    pub quote_bg: Rgba,
    /// 表头底色（明 #f2f2f2 / 暗 #323232）
    pub table_head_bg: Rgba,
    /// Markdown 标题文字色（编辑器滤镜用，明 #0b57d0 / 暗 #6fb1ff）
    pub heading: Rgba,
    /// 悬浮小卡片/弹层背景（EPUB 脚注卡片等，明 #ffffff / 暗 #333333）
    pub popover_bg: Rgba,
    /// Mermaid 图节点分类 accent 色板（6 色 Fluent 语义色，亮/暗各一套：亮底取深一档、
    /// 暗底取亮一档，保证两种卡底色下描边对比达标）。节点按出现顺序循环取色，
    /// 作者 classDef/style 显式 hex 色不参与循环。色相参考 zed 玩家板（蓝/绿/橙/紫/红/青）。
    pub mermaid_accent_palette: [Rgba; 6],
    /// Mermaid 边线/边框主色（≈ divider 同 hue 提一档：中性灰、比 divider 更可辨；
    /// 亮/暗随本 token 自动分流，paint 侧再按 a≈0.85 弱化）。
    pub mermaid_border: Rgba,
    /// 编辑器选区底色 = **半透明补偿色**：叠到编辑区底上复合成目标取色值（暗
    /// `rgba(0x3ab5e973)`≈#245C74 / 亮 `rgba(0x619bd373)`≈#B8D2EB）。用半透明而非
    /// 不透明，保证**括号竖线穿过选区仍能透出**（竖线叠层在文字/选区层之下）；非竖线
    /// 像素复合后与不透明目标色一致。消费方（`CodeEditor::refresh_metrics`）直读本
    /// token，零裸色四处散落。
    pub editor_selection_bg: Rgba,
    /// 编辑器已折叠行的整行底色（暗 #172931 冷灰蓝 / 亮 #E6EFF8），比选区更淡、
    /// 更贴内容底，一眼认出折叠位置、又不喧宾夺主。
    pub editor_fold_bg: Rgba,
    /// 编辑器光标所在行（selection head 所在行、且持焦且选区坍缩）的整行高亮底
    /// （暗 #242525 中性灰 / 亮 #E6EFF8）；暗色与折叠行（#172931）区分开。稳定持焦
    /// 高亮，不受光标闪烁影响；存在非坍缩选区时不高亮整行（只高亮选中字符本身）。
    pub editor_active_line_bg: Rgba,
    /// 编辑器「查找」（Ctrl+F）**非当前**匹配的字符底色（明 #FFE497 / 暗 #4A3F16，
    /// 弱黄系：一眼看出全部命中、又明显弱于「当前命中」的强橙）。
    pub editor_find_match_bg: Rgba,
    /// 编辑器「查找」**当前命中**（导航所指）的字符底色（明 #FFB703 / 暗 #FFB703 同
    /// 一饱和橙黄，比非当前更醒目，与选区/折叠底都可辨）。
    pub editor_find_current_bg: Rgba,
}

impl ColorScheme {
    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            surface: rgb(0xffffff),
            fg: rgb(0x202020),
            fg_muted: rgb(0x5f5f5f),
            border: rgba(0x00000017),
            // Fluent 亮色系统强调主色 = #005FB8（亮底深一档默认蓝，白底文字/图标对比达标）
            accent: rgb(0x005fb8),
            link: rgb(0x0b57d0),
            error: rgb(0xc42b1c),
            // 禁用文字 = Fluent 40% 黑叠白底（约 #999），比 fg_muted(#5f5f5f) 更弱一档。
            fg_disabled: rgb(0x9a9a9a),
            // 聚焦框 = 亮档强调色同源（Fluent 亮底强调蓝）。
            focus_ring: rgb(0x005fb8),
            // 按钮选中/悬停 = 前景黑低透明（原 fg.opacity(0.08)，同值收编）。
            selected_bg: rgba(0x00000014),
            // 按压 = 再深一档（原 fg.opacity(0.14)）。
            pressed_bg: rgba(0x00000024),
            // 分栏/调宽强调线 = 亮档强调色同源。
            drag_hover_border: rgb(0x005fb8),
            code_block_bg: rgb(0xf5f5f5),
            code_inline_fg: rgb(0x202020),
            code_inline_bg: rgb(0xf0f0f0),
            editor_code_bg: rgb(0xf2f2f2),
            quote_bg: rgb(0xf7f7f7),
            table_head_bg: rgb(0xf2f2f2),
            heading: rgb(0x0b57d0),
            popover_bg: rgb(0xffffff),
            // Mermaid 分类色板（亮底）：6 色相分散、亮度取深一档（白/浅灰卡上描边对比达标）。
            mermaid_accent_palette: [
                hsla(207.8 / 360., 0.78, 0.40, 1.0).into(),
                hsla(95.0 / 360., 0.45, 0.36, 1.0).into(),
                hsla(29.0 / 360., 0.68, 0.42, 1.0).into(),
                hsla(286.0 / 360., 0.50, 0.40, 1.0).into(),
                hsla(355.0 / 360., 0.62, 0.42, 1.0).into(),
                hsla(187.0 / 360., 0.55, 0.36, 1.0).into(),
            ],
            mermaid_border: rgb(0x909090),
            editor_selection_bg: rgba(0x619bd373),
            editor_fold_bg: rgb(0xe6eff8),
            editor_active_line_bg: rgb(0xe6eff8),
            editor_find_match_bg: rgb(0xffe497),
            editor_find_current_bg: rgb(0xffb703),
        }
    }

    pub fn dark() -> Self {
        Self {
            bg: rgb(0x2b2b2b),
            surface: rgb(0x333333),
            fg: rgb(0xe6e6e6),
            fg_muted: rgb(0x9d9d9d),
            border: rgba(0xffffff1a),
            // Fluent/Win11 暗色系统强调主色 = #60CDFF（暗底亮一档默认蓝，深底文字/图标对比达标；
            // hover/active/selection 由消费方按本 accent 派生，自动跟随）
            accent: rgb(0x60cdff),
            link: rgb(0x6fb1ff),
            error: rgb(0xf16a6a),
            // 禁用文字 = Fluent 40% 白叠 #2b2b2b（约 #7d），比 fg_muted(#9d9d9d) 更弱一档。
            fg_disabled: rgb(0x777777),
            // 聚焦框 = 暗档强调色同源（Win11 暗底亮蓝）。
            focus_ring: rgb(0x60cdff),
            // 按钮选中/悬停 = 前景白低透明（原 fg.opacity(0.14)，同值收编）。
            selected_bg: rgba(0xffffff24),
            // 按压 = 再深一档（原 fg.opacity(0.24)）。
            pressed_bg: rgba(0xffffff3d),
            // 分栏/调宽强调线 = 暗档强调色同源。
            drag_hover_border: rgb(0x60cdff),
            code_block_bg: rgb(0x262626),
            code_inline_fg: rgb(0xe8e8e8),
            code_inline_bg: rgb(0x3a3a3a),
            editor_code_bg: rgb(0x2e2e2e),
            quote_bg: rgb(0x262626),
            table_head_bg: rgb(0x323232),
            heading: rgb(0x6fb1ff),
            popover_bg: rgb(0x333333),
            // Mermaid 分类色板（暗底）：色相同亮档、亮度取亮一档（zed 暗色玩家板同款，
            // 深色卡上描边对比达标）。
            mermaid_accent_palette: [
                hsla(207.8 / 360., 0.81, 0.66, 1.0).into(),
                hsla(95.0 / 360., 0.38, 0.62, 1.0).into(),
                hsla(29.0 / 360., 0.54, 0.61, 1.0).into(),
                hsla(286.0 / 360., 0.51, 0.64, 1.0).into(),
                hsla(355.0 / 360., 0.65, 0.65, 1.0).into(),
                hsla(187.0 / 360., 0.47, 0.55, 1.0).into(),
            ],
            mermaid_border: rgb(0xa8a8a8),
            editor_selection_bg: rgba(0x3ab5e973),
            editor_fold_bg: rgb(0x172931),
            editor_active_line_bg: rgb(0x242525),
            editor_find_match_bg: rgb(0x4a3f16),
            editor_find_current_bg: rgb(0xffb703),
        }
    }
}

/// 外壳（App 框架）配色：标题栏/菜单栏/选项卡栏/侧边栏/状态栏等**窗口外壳**的语义色，
/// 与 [`ColorScheme`]（内容渲染色板）平行——前者管「框」，后者管「内容」。
///
/// **Win11 Fluent 中性外壳色板**：以中性灰（亮 #F5F5F5 / 暗 #202020）打底，配合
/// VS Code 工作台层次（灰带 + 白色卡片选项卡 + 8px 圆角浮动层）。hover / 按压 / 关闭/
/// 拖拽 等交互色单独成字段，统一由组件像素级消费，禁止在组件里新写硬编码 `rgba()`。
///
/// **与 [`ColorScheme`] 同源**：同名字段（`panel_bg`/`bar_border`/`text_primary`/
/// `text_minor`/`chip_text`/`tab_active_bg`/`tab_active_border`/`pill_fg`/`dirty`）
/// 直接引用 `ColorScheme` 对应字段，物理同值、不会漂移（见
/// `chrome_and_color_scheme_share_ramp` 测试）。其余字段是外壳专属层，自定中性灰阶。
#[derive(Clone, Copy, Debug)]
pub struct ChromeColors {
    /// 编辑区以外的画布背景（＝窗口底色）
    pub window_bg: Rgba,
    /// 自绘材质（假 Mica/acrylic）：窗口基底垂向渐变的顶端色（应用内绘制，不依赖系统）
    pub material_top: Rgba,
    /// 自绘材质：垂向渐变底端色
    pub material_bottom: Rgba,
    /// 菜单栏 / 选项卡栏 / 状态栏等工具条背景
    pub bar_bg: Rgba,
    /// 工具条分隔边框
    pub bar_border: Rgba,
    /// 区域分隔发丝线（**不透明**、全屏一致——半透明叠不同底色会混出各自的灰色；
    /// 标题栏底 / 编辑区顶边 / 侧栏右缘 / 状态栏边框共用；原 `config::layout`
    /// `SEPARATOR_LIGHT/DARK`，亮 #D9D9D9 / 暗 #2A2B2C）
    pub hairline: Rgba,
    /// 编辑器/预览面板背景
    pub panel_bg: Rgba,
    /// **内容区真实渲染底**（编辑区/预览/欢迎页）：暗 #121314（收编自 `config::layout`
    /// `EDITOR_BG_DARK`，常量已删）/ 亮 #FFFFFF。与 `panel_bg` **不同源、不互相替代**：
    /// `panel_bg` 是外壳层「面板泛化底」（随 `colors.bg`，暗 #2B2B2B，
    /// 供既有不变式原样保留）；`editor_bg` 才是屏幕上真正铺到编辑区/欢迎页的底色，
    /// 统一经 [`crate::ui::shell::content_bg`] 取用。
    pub editor_bg: Rgba,
    /// **外壳 chrome 区域底色**（标题栏/选项卡栏/侧栏/状态栏）：暗 #191A1B（收编自
    /// `config::layout` `CHROME_BG_DARK`，常量已删）/ 亮 #FFFFFF（= `surface` 亮档同值）。
    /// Mica 半透明版（`mica_bar_bg` 亮档）仍按原公式 `bar_bg() * alpha` 合成，不用本字段。
    pub chrome_bg: Rgba,
    /// 主要文字
    pub text_primary: Rgba,
    /// 次要/弱化文字
    pub text_minor: Rgba,
    /// 三级文字（菜单快捷键 / 未激活图标，亮暗同值）
    pub text_tertiary: Rgba,
    /// 胶囊/小按钮等「芯片」背景（写入于明/暗色构造；读取仅在测试断言 `pill_bg==chip_bg`，
    /// 非测试构建不读 → 标记允许 dead_code 避免 unused 警告）
    #[allow(dead_code)]
    pub chip_bg: Rgba,
    /// 芯片上的文字
    pub chip_text: Rgba,
    /// 激活选项卡背景
    pub tab_active_bg: Rgba,
    /// 激活选项卡边框
    pub tab_active_border: Rgba,
    /// 状态栏语言徽标 / 全屏按钮等的 pill 底色（＝芯片底，随 `chip_*` 令牌）
    pub pill_bg: Rgba,
    /// pill 前景文字（＝芯片字）
    pub pill_fg: Rgba,
    /// 文件脏标记点（亮暗同值）
    pub dirty: Rgba,
    /// 通用悬停填充（列表行 / 图标钮 / 选项卡 hover）
    pub hover_fill: Rgba,
    /// 通用按压填充（比 hover 深一档）
    pub pressed_fill: Rgba,
    /// 关闭按钮悬停填充（危险红，亮暗同值）
    pub close_hover: Rgba,
    /// 关闭按钮按压填充（危险红深一档，亮暗同值）
    pub close_pressed: Rgba,
    /// 选项卡拖拽「幽灵」胶囊底色（亮暗 = 对应档强调色）
    pub drag_ghost_bg: Rgba,
    /// 菜单项选中/悬停高亮（Win11 中性灰填充，替代强调蓝 tint）
    pub menu_highlight: Rgba,
    /// 横向滚动条滑块底色（半透明中性灰，亮暗同值；原 shell 裸色 `rgba(0x6060604a)`）
    pub scrollbar_thumb: Rgba,
}

/// 自绘玻璃材质 chrome 透明度：标题栏/选项卡栏/侧栏/状态栏/窗口底用
/// （半透明才能显出应用内自绘材质层，同时保留文字对比。不依赖系统 Mica/acrylic）。
pub const MICA_CHROME_ALPHA_LIGHT: f32 = 0.86;
pub const MICA_CHROME_ALPHA_DARK: f32 = 0.82;
/// 浮动层（菜单/通知等）自绘玻璃材质透明度（不依赖系统 acrylic）。
pub const FLOAT_ACRYLIC_ALPHA_LIGHT: f32 = 0.92;
pub const FLOAT_ACRYLIC_ALPHA_DARK: f32 = 0.94;

impl ChromeColors {
    pub fn light() -> Self {
        let c = ColorScheme::light();
        Self {
            window_bg: rgb(0xf5f5f5),
            material_top: rgb(0xf8fafc),
            material_bottom: rgb(0xedeff4),
            bar_bg: rgb(0xf5f5f5),
            bar_border: c.border,
            // 区域分隔发丝线（不透明，亮浅灰 #D9D9D9，原 config SEPARATOR_LIGHT）
            hairline: rgb(0xd9d9d9),
            panel_bg: c.bg,
            // 内容区真实底（亮 = panel_bg 亮档 #FFFFFF，同值）
            editor_bg: rgb(0xffffff),
            // 外壳 chrome 区域底（亮 = 原 card_surface 亮档 surface #FFFFFF，同值）
            chrome_bg: rgb(0xffffff),
            text_primary: c.fg,
            text_minor: c.fg_muted,
            text_tertiary: rgb(0x8a8a8a),
            chip_bg: rgb(0xe8e8e8),
            chip_text: c.fg,
            tab_active_bg: c.bg,
            tab_active_border: c.border,
            pill_bg: rgb(0xe8e8e8),
            pill_fg: c.fg,
            dirty: c.error,
            hover_fill: rgba(0x0000000d),
            pressed_fill: rgba(0x00000017),
            close_hover: rgb(0xe81123),
            close_pressed: rgb(0xa70d1a),
            drag_ghost_bg: c.accent,
            menu_highlight: rgba(0x00000014),
            // 滚动条滑块 = 半透明中性灰（亮暗同值，原 shell rgba(0x6060604a)）
            scrollbar_thumb: rgba(0x6060604a),
        }
    }

    pub fn dark() -> Self {
        let c = ColorScheme::dark();
        Self {
            window_bg: rgb(0x202020),
            material_top: rgb(0x24262b),
            material_bottom: rgb(0x1b1d21),
            bar_bg: rgb(0x202020),
            bar_border: c.border,
            // 区域分隔发丝线（不透明，暗深灰 #2A2B2C，原 config SEPARATOR_DARK）
            hairline: rgb(0x2a2b2c),
            panel_bg: c.bg,
            // 内容区真实底（暗 = #121314，原 config EDITOR_BG_DARK，此前散落
            // ui::shell::content_bg 的裸 hex）
            editor_bg: rgb(0x121314),
            // 外壳 chrome 区域底（暗 = #191A1B，原 config CHROME_BG_DARK，此前散落
            // card_surface / mica_bar_bg 的裸 hex）
            chrome_bg: rgb(0x191a1b),
            text_primary: c.fg,
            text_minor: c.fg_muted,
            text_tertiary: rgb(0x6e6e6e),
            chip_bg: rgb(0x3a3a3a),
            chip_text: c.fg,
            tab_active_bg: c.bg,
            tab_active_border: c.border,
            pill_bg: rgb(0x3a3a3a),
            pill_fg: c.fg,
            dirty: c.error,
            hover_fill: rgba(0xffffff14),
            pressed_fill: rgba(0xffffff24),
            close_hover: rgb(0xe81123),
            close_pressed: rgb(0xa70d1a),
            drag_ghost_bg: c.accent,
            menu_highlight: rgba(0xffffff1a),
            // 滚动条滑块 = 半透明中性灰（亮暗同值，原 shell rgba(0x6060604a)）
            scrollbar_thumb: rgba(0x6060604a),
        }
    }

    /// 按明暗模式取对应一套（无 App 上下文时的便捷入口）。
    pub fn for_mode(dark: bool) -> Self {
        if dark {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

/// 全局主题：当前模式 + 对应语义色板 + 排版参数。
#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: ThemeMode,
    pub colors: ColorScheme,
    pub typography: Typography,
}

impl Global for Theme {}

/// 未初始化时的兜底主题（Light，与应用默认一致）。
static DEFAULT_LIGHT: LazyLock<Theme> = LazyLock::new(Theme::light);

impl Theme {
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            colors: ColorScheme::light(),
            typography: Typography::default(),
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: ColorScheme::dark(),
            typography: Typography::default(),
        }
    }

    pub fn is_dark(&self) -> bool {
        matches!(self.mode, ThemeMode::Dark)
    }

    /// 读取全局主题。未初始化时回落到默认 Light（测试/早期初始化顺序不敏感）。
    pub fn global(cx: &App) -> &Theme {
        cx.try_global::<Theme>().unwrap_or(&DEFAULT_LIGHT)
    }

    /// 切换主题：写本全局 + 刷新窗口。
    pub fn set(mode: ThemeMode, cx: &mut App) {
        let theme = match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
        };
        cx.set_global(theme);
        cx.refresh_windows();
    }
}

/// 状态栏语言徽标的「徽记色点」色相（VS Code 式按语言类别分类着色）。
/// 只存色相（0..=1 圈，见 [`gpui::Hsla::h`]），饱和度/亮度由
/// [`language_badge_color`] 统一派生，避免在状态栏组件里散落硬编码 `rgba`。
/// 第二项 `vivid`：有明确语言身份时用高饱和辨识色；纯文本/未知类别走中性灰蓝。
fn language_badge_hue(label: &str) -> (f32, bool) {
    match label {
        "Markdown" => (0.60, true),   // 蓝
        "Rust" => (0.08, true),       // 橙
        "Python" => (0.55, true),     // 青
        "JavaScript" => (0.14, true), // 黄
        "HTML" => (0.06, true),       // 橙红
        "CSS" => (0.68, true),        // 靛
        "C" | "C++" => (0.04, true),  // 红橙
        "Go" => (0.40, true),         // 绿
        "Java" => (0.10, true),       // 琥珀
        "Shell" => (0.58, true),      // 蓝
        "YAML" => (0.94, true),       // 玫红
        "SQL" => (0.76, true),        // 紫
        "XML" => (0.64, true),        // 靛蓝
        "TOML" => (0.30, true),       // 黄绿
        "JSON" => (0.22, true),       // 土黄
        "EPUB" => (0.78, true),       // 紫
        "PDF" => (0.02, true),        // 红
        "图片" => (0.86, true),        // 品红
        _ => (0.60, false),           // 纯文本/文本：低饱和中性灰蓝
    }
}

/// 状态栏语言徽标「徽记色点」颜色：VS Code 式浓郁辨识色，明/暗两种底色下都可辨。
///
/// 亮底 pill（`#E8E8E8`）取深一档亮度、暗底 pill（`#3A3A3A`）取亮一档亮度，保证
/// 点在小圆底上轮廓分明；无语言身份（纯文本/未知）时降饱和为中性灰蓝。
pub fn language_badge_color(label: &str, dark: bool) -> Hsla {
    let (h, vivid) = language_badge_hue(label);
    let (s, l) = if vivid {
        if dark { (0.70, 0.70) } else { (0.68, 0.40) }
    } else if dark {
        (0.16, 0.72)
    } else {
        (0.16, 0.42)
    };
    hsla(h, s, l, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn uninitialized_defaults_to_light(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let t = Theme::global(cx);
            assert_eq!(t.mode, ThemeMode::Light);
            assert!(!t.is_dark());
        });
    }

    #[gpui::test]
    fn set_writes_mode_and_refreshes(cx: &mut TestAppContext) {
        cx.update(|cx| Theme::set(ThemeMode::Dark, cx));
        cx.update(|cx| {
            assert!(Theme::global(cx).is_dark());
        });
        cx.update(|cx| Theme::set(ThemeMode::Light, cx));
        cx.update(|cx| {
            assert!(!Theme::global(cx).is_dark());
        });
    }

    #[gpui::test]
    fn palette_has_distinct_light_and_dark(_cx: &mut TestAppContext) {
        let light = Theme::light();
        let dark = Theme::dark();
        assert_ne!(light.colors.bg, dark.colors.bg, "亮/暗主背景应不同");
        assert_ne!(light.colors.fg, dark.colors.fg, "亮/暗主文字应不同");
        assert_ne!(light.colors.fg, light.colors.fg_muted, "主次文字应不同");
        // Fluent 双套强调色：亮底用深蓝、暗底用亮蓝（微软规范，保证两种底色下对比达标）。
        assert_ne!(light.colors.accent, dark.colors.accent, "亮/暗强调色不再统一（Fluent 亮深蓝 vs 暗亮蓝）");
        assert_eq!(light.colors.accent, rgb(0x005fb8), "亮主题强调色=#005FB8（Fluent 亮系默认蓝）");
        assert_eq!(dark.colors.accent, rgb(0x60cdff), "暗主题强调色=#60CDFF（Win11 暗色亮蓝）");
        assert_ne!(light.colors.link, dark.colors.link, "链接色随明暗切换");
    }

    #[gpui::test]
    fn markdown_semantic_palette_pins_ms_v2_values(_cx: &mut TestAppContext) {
        // 回归：第 5 步「亮暗双主题配色谐波化」把 Markdown 语义色板换到统一中性灰阶
        // + 蓝色系（剔掉旧 GitHub/slate 蓝灰），逐字节钉死新值。
        // 预览/编辑器/WYSIWYG 依赖这些精确值（见各模块从 theme 派生 / 单测）。
        let light = Theme::light();
        let dark = Theme::dark();
        // 代码块背景（亮 #F5F5F5 / 暗 #262626）
        assert_eq!(light.colors.code_block_bg, rgb(0xf5f5f5), "亮主题代码块底");
        assert_eq!(dark.colors.code_block_bg, rgb(0x262626), "暗主题代码块底");
        // 行内代码 pill（亮 底 #F0F0F0 + 字 #202020 / 暗 底 #3A3A3A + 字 #E8E8E8）
        assert_eq!(light.colors.code_inline_bg, rgb(0xf0f0f0));
        assert_eq!(light.colors.code_inline_fg, rgb(0x202020));
        assert_eq!(dark.colors.code_inline_bg, rgb(0x3a3a3a));
        assert_eq!(dark.colors.code_inline_fg, rgb(0xe8e8e8));
        // 编辑器滤镜行内代码底（亮 #F2F2F2 / 暗 #2E2E2E，与预览略异）
        assert_eq!(light.colors.editor_code_bg, rgb(0xf2f2f2));
        assert_eq!(dark.colors.editor_code_bg, rgb(0x2e2e2e));
        // 引述底 / 表头底随明暗切换，且同档里互不相同（层次可辨）
        assert_ne!(light.colors.quote_bg, dark.colors.quote_bg, "引述底色随明暗");
        assert_ne!(light.colors.table_head_bg, dark.colors.table_head_bg, "表头底色随明暗");
        assert_ne!(light.colors.quote_bg, light.colors.table_head_bg, "亮引述≠亮表头（#F7F7F7 vs #F2F2F2）");
        assert_ne!(dark.colors.quote_bg, dark.colors.table_head_bg, "暗引述≠暗表头（#262626 vs #323232）");
        // 标题/链接/弹层同源蓝系，随明暗切换
        assert_eq!(light.colors.heading, light.colors.link, "亮标题=亮链接同蓝");
        assert_eq!(dark.colors.heading, dark.colors.link, "暗标题=暗链接同蓝");
        assert_ne!(light.colors.heading, dark.colors.heading, "标题色随明暗");
        // 悬浮卡片底随明暗（亮 #FFFFFF / 暗 #333333）
        assert_ne!(light.colors.popover_bg, dark.colors.popover_bg, "弹层底色随明暗");
    }

    #[gpui::test]
    fn mermaid_accent_palette_is_distinct_and_mode_split(_cx: &mut TestAppContext) {
        // Mermaid 分类 accent 板：亮/暗各 6 色、同下标色相一致、暗档比亮档亮（对比达标）、
        // 六色色相彼此分散；mermaid_border 随明暗分流。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(light.mermaid_accent_palette.len(), 6, "亮板 6 色");
        assert_eq!(dark.mermaid_accent_palette.len(), 6, "暗板 6 色");
        for i in 0..6 {
            let l: Hsla = light.mermaid_accent_palette[i].into();
            let d: Hsla = dark.mermaid_accent_palette[i].into();
            assert!(
                (l.h - d.h).abs() < 1e-3,
                "色相应跨明暗一致（同 hue 板）：[{i}]"
            );
            assert!(d.l > l.l, "暗档应比亮档亮（深底需亮描边）：[{i}]");
        }
        let hsl: Vec<Hsla> = light
            .mermaid_accent_palette
            .iter()
            .map(|&c| c.into())
            .collect();
        for i in 0..6 {
            for j in (i + 1)..6 {
                let dh = (hsl[i].h - hsl[j].h).abs();
                assert!(
                    dh > 0.02 && dh < 0.98,
                    "六色色相应分散可辨：[{i}] vs [{j}] Δh={dh}"
                );
            }
        }
        assert_ne!(light.mermaid_border, dark.mermaid_border, "边线色随明暗分流");
    }

    #[gpui::test]
    fn editor_row_bg_tokens_pinned_both_themes(_cx: &mut TestAppContext) {
        // 编辑器行底色三 token 逐字节钉死：选区为**半透明补偿色**（叠编辑区底合成目标
        // 取色值：暗 rgba(0x3ab5e973)≈#245C74 / 亮 rgba(0x619bd373)≈#B8D2EB，保证括号
        // 竖线穿选区透出），折叠/光标行保持不透明（暗 #172931/#242525、亮 #E6EFF8×2）。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(dark.editor_selection_bg, rgba(0x3ab5e973), "暗选区底（半透明补偿色）");
        assert_eq!(dark.editor_fold_bg, rgb(0x172931), "暗折叠行底色");
        assert_eq!(dark.editor_active_line_bg, rgb(0x242525), "暗光标行底色（已与折叠行区分）");
        assert_eq!(light.editor_selection_bg, rgba(0x619bd373), "亮选区底（半透明补偿色）");
        assert_eq!(light.editor_fold_bg, rgb(0xe6eff8), "亮折叠行底色");
        assert_eq!(light.editor_active_line_bg, rgb(0xe6eff8), "亮光标行底色 = 折叠行同色");
        // 选区半透明（竖线透出契约）；折叠/光标行不透明不动。
        assert!(dark.editor_selection_bg.a < 1.0, "暗选区底应半透明（a<1）");
        assert!(light.editor_selection_bg.a < 1.0, "亮选区底应半透明（a<1）");
        assert_eq!(dark.editor_fold_bg.a, 1.0, "暗折叠底应不透明");
        assert_eq!(dark.editor_active_line_bg.a, 1.0, "暗光标行底应不透明");
        // 语义不变式：亮色光标行底色恒等于折叠行底色；暗色三档两两互不相同（折叠冷灰蓝
        // #172931 / 光标行中性灰 #242525 / 选区半透明蓝系另档）。两主题都「折叠/光标
        // 行比选区更贴背景」。
        assert_ne!(dark.editor_selection_bg, dark.editor_fold_bg, "暗：选区 ≠ 折叠");
        assert_ne!(dark.editor_selection_bg, dark.editor_active_line_bg, "暗：选区 ≠ 光标行");
        assert_ne!(dark.editor_fold_bg, dark.editor_active_line_bg, "暗：折叠 ≠ 光标行（两色已区分）");
        assert_eq!(light.editor_active_line_bg, light.editor_fold_bg, "亮：光标行=折叠行");
        // 同同档值随明暗分离（暗深 / 亮浅）。
        assert_ne!(dark.editor_selection_bg, light.editor_selection_bg, "选区底色随明暗");
        assert_ne!(dark.editor_fold_bg, light.editor_fold_bg, "折叠底色随明暗");
        assert_ne!(dark.editor_active_line_bg, light.editor_active_line_bg, "光标行底色随明暗");
    }

    #[gpui::test]
    fn find_highlight_tokens_pinned_both_themes(_cx: &mut TestAppContext) {
        // 编辑器查找高亮两 token：当前命中（强橙）与非当前（弱黄）亮暗两套都钉死，
        // 且「当前 ≠ 非当前」保证导航所指一目了然。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(light.editor_find_match_bg, rgb(0xffe497), "亮非当前=弱黄");
        assert_eq!(light.editor_find_current_bg, rgb(0xffb703), "亮当前=强橙");
        assert_eq!(dark.editor_find_match_bg, rgb(0x4a3f16), "暗非当前=弱黄暗档");
        assert_eq!(dark.editor_find_current_bg, rgb(0xffb703), "暗当前=强橙");
        assert_ne!(
            light.editor_find_match_bg, light.editor_find_current_bg,
            "亮：当前 ≠ 非当前（同档可辨）"
        );
        assert_ne!(
            dark.editor_find_match_bg, dark.editor_find_current_bg,
            "暗：当前 ≠ 非当前（同档可辨）"
        );
    }

    #[gpui::test]
    fn chrome_palette_has_distinct_light_and_dark(_cx: &mut TestAppContext) {
        // 第 1 步把 App 外壳 11 个 helper 收编为 ChromeColors：亮/暗外壳底色必须分离。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_ne!(light.window_bg, dark.window_bg, "亮/暗窗口背景应不同");
        assert_ne!(light.bar_bg, dark.bar_bg, "亮/暗工具条背景应不同");
        assert_ne!(light.panel_bg, dark.panel_bg, "亮/暗面板背景应不同");
    }

    #[gpui::test]
    fn chrome_palette_pins_ms_v2_shell_values(_cx: &mut TestAppContext) {
        // 回归：第 5 步谐波化后的 Win11 Fluent 中性外壳关键值钉死（与 ColorScheme 同源）。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_eq!(light.window_bg, rgb(0xf5f5f5), "亮窗口背景=中性灰带");
        assert_eq!(light.bar_bg, rgb(0xf5f5f5), "亮工具条背景=窗口底");
        assert_eq!(light.panel_bg, rgb(0xffffff), "亮面板背景=白");
        assert_eq!(light.pill_bg, rgb(0xe8e8e8), "亮 pill 底色=chip 底");
        assert_eq!(dark.window_bg, rgb(0x202020), "暗窗口背景");
        assert_eq!(dark.panel_bg, rgb(0x2b2b2b), "暗面板背景=内容 bg");
        assert_eq!(dark.bar_border, rgba(0xffffff1a), "暗工具条分隔边框=内容 border");
        // pill 跟随 chip 令牌（不再是旧的主蓝高亮胶囊）
        assert_eq!(light.pill_bg, light.chip_bg, "亮 pill=chip 底");
        assert_eq!(light.pill_fg, light.chip_text, "亮 pill=chip 字");
        assert_eq!(dark.pill_bg, dark.chip_bg, "暗 pill=chip 底");
        assert_eq!(dark.pill_fg, dark.chip_text, "暗 pill=chip 字");
        // 交互两档：hover / pressed 亮暗分离、pressed 与 hover 不同、关闭红亮暗同值
        assert_ne!(light.hover_fill, dark.hover_fill, "hover 填充随明暗");
        assert_ne!(light.pressed_fill, dark.pressed_fill, "按压填充随明暗");
        assert_ne!(light.hover_fill, light.pressed_fill, "hover 与按压深浅不同");
        assert_eq!(light.close_hover, dark.close_hover, "关闭红亮暗同值");
        // 菜单高亮：中性灰随明暗（亮半透明黑 / 暗半透明白）
        assert_ne!(light.menu_highlight, dark.menu_highlight, "菜单高亮随明暗");
    }

    #[gpui::test]
    fn state_colors_are_consistent_and_distinct(_cx: &mut TestAppContext) {
        // P1-7：状态语义色——禁用/选中/按压/聚焦/拖拽各态亮暗两套可取、且与邻近色可辨。
        let light = Theme::light();
        let dark = Theme::dark();
        let (lc, dc) = (light.colors, dark.colors);
        // 禁用文字：亮暗不同，且比 fg_muted 更弱（亮度离得更远），又不能浅到与底色无别。
        assert_ne!(lc.fg_disabled, dc.fg_disabled, "禁用文字随明暗");
        assert_ne!(lc.fg_disabled, lc.fg_muted, "亮禁用 ≠ 亮次要文字");
        assert_ne!(dc.fg_disabled, dc.fg_muted, "暗禁用 ≠ 暗次要文字");
        assert_ne!(lc.fg_disabled, lc.fg, "亮禁用 ≠ 主文字");
        assert_ne!(dc.fg_disabled, dc.fg, "暗禁用 ≠ 主文字");
        // 聚焦框 = 各档强调色同源（Fluent 亮深蓝 / 暗亮蓝）。
        assert_eq!(lc.focus_ring, lc.accent, "亮聚焦框 = 亮强调色");
        assert_eq!(dc.focus_ring, dc.accent, "暗聚焦框 = 暗强调色");
        assert_ne!(lc.focus_ring, dc.focus_ring, "聚焦框随明暗");
        // 选中/按压基底：预置 alpha，按压比选中深一档、且亮暗分离（明黑 / 暗白）。
        assert_ne!(lc.selected_bg, lc.pressed_bg, "亮选中与按压深浅不同");
        assert_ne!(dc.selected_bg, dc.pressed_bg, "暗选中与按压深浅不同");
        assert_ne!(lc.selected_bg, dc.selected_bg, "选中基底随明暗");
        assert_ne!(lc.pressed_bg, dc.pressed_bg, "按压基底随明暗");
        assert!(lc.selected_bg.a < lc.pressed_bg.a, "亮按压 alpha 应大于选中");
        assert!(dc.selected_bg.a < dc.pressed_bg.a, "暗按压 alpha 应大于选中");
        // 拖拽强调线 = 各档强调色同源。
        assert_eq!(lc.drag_hover_border, lc.accent, "亮拖拽线 = 亮强调色");
        assert_eq!(dc.drag_hover_border, dc.accent, "暗拖拽线 = 暗强调色");
    }

    #[gpui::test]
    fn hairline_and_scrollbar_thumb_pinned_light_dark(_cx: &mut TestAppContext) {
        // 收编 `config::layout` SEPARATOR_LIGHT/DARK（区域分隔发丝线）与 shell 滚动条
        // 裸色 rgba(0x6060604a)：亮/暗各自值 = 收编前原值，行为零回归。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_eq!(light.hairline, rgb(0xd9d9d9), "亮发丝线=原 SEPARATOR_LIGHT #D9D9D9");
        assert_eq!(dark.hairline, rgb(0x2a2b2c), "暗发丝线=原 SEPARATOR_DARK #2A2B2C");
        assert_ne!(light.hairline, dark.hairline, "发丝线随明暗切换");
        assert_eq!(
            light.scrollbar_thumb,
            rgba(0x6060604a),
            "滚动条滑块=原 shell rgba(0x6060604a)"
        );
        assert_eq!(
            dark.scrollbar_thumb,
            rgba(0x6060604a),
            "滚动条滑块亮暗同值"
        );
        assert_eq!(
            light.scrollbar_thumb,
            dark.scrollbar_thumb,
            "滚动条滑块亮暗持同值（亮暗同值字段）"
        );
    }

    #[gpui::test]
    fn chrome_state_colors_are_pinned(_cx: &mut TestAppContext) {
        // P1-7：外壳层状态色钉死——关闭按压红亮暗同值；拖拽幽灵随明暗且 = 强调色。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_eq!(light.close_pressed, rgb(0xa70d1a), "亮关闭按压红");
        assert_eq!(dark.close_pressed, rgb(0xa70d1a), "暗关闭按压红 = 亮档（危险红同值）");
        assert_eq!(light.close_pressed, dark.close_pressed, "关闭按压红亮暗同值");
        assert_ne!(light.close_pressed, light.close_hover, "按压比 hover 深一档");
        assert_eq!(light.drag_ghost_bg, ColorScheme::light().accent, "亮拖拽幽灵 = 亮强调色");
        assert_eq!(dark.drag_ghost_bg, ColorScheme::dark().accent, "暗拖拽幽灵 = 暗强调色");
        assert_ne!(light.drag_ghost_bg, dark.drag_ghost_bg, "拖拽幽灵随明暗");
    }

    #[gpui::test]
    fn editor_chrome_bg_tokens_pinned_light_dark(_cx: &mut TestAppContext) {
        // 颜色收编专项：内容区底 / 外壳 chrome 底收编为 theme 语义 token（删 config
        // `EDITOR_BG_DARK` / `CHROME_BG_DARK`）。取值 = 原常量逐字节（暗 #121314 / #191A1B，
        // 亮 #FFFFFF），钉死防回归；并与面板泛化底 `panel_bg`（随 colors.bg）语义区分，
        // 两 token 不互相替代。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_eq!(light.editor_bg, rgb(0xffffff), "亮编辑区底=白");
        assert_eq!(light.chrome_bg, rgb(0xffffff), "亮外壳底=白");
        assert_eq!(dark.editor_bg, rgb(0x121314), "暗编辑区底=原 EDITOR_BG_DARK");
        assert_eq!(dark.chrome_bg, rgb(0x191a1b), "暗外壳底=原 CHROME_BG_DARK");
        assert_ne!(light.editor_bg, dark.editor_bg, "编辑区底随明暗");
        assert_ne!(light.chrome_bg, dark.chrome_bg, "外壳底随明暗");
        // 与保留的 panel_bg 分道：暗档 editor_bg(#121314) ≠ panel_bg(#2B2B2B)，
        // panel_bg 及其不变式逐字节保留（不互相替代）。
        assert_eq!(dark.panel_bg, rgb(0x2b2b2b), "暗 panel_bg 原样保留（随 colors.bg）");
        assert_ne!(dark.editor_bg, dark.panel_bg, "暗编辑区底 ≠ 暗 panel_bg（语义分离）");
    }

    #[gpui::test]
    fn chrome_and_color_scheme_share_ramp(_cx: &mut TestAppContext) {
        // 第 5 步核心不变式：外壳（ChromeColors）与内容（ColorScheme）**物理同源**，
        // 同名字段必须逐字节相等——改调色板只改一处，双方一起变、永不漂移。
        for (chrome, colors) in [
            (ChromeColors::light(), ColorScheme::light()),
            (ChromeColors::dark(), ColorScheme::dark()),
        ] {
            assert_eq!(chrome.panel_bg, colors.bg, "面板背景=内容 bg");
            assert_eq!(chrome.bar_border, colors.border, "工具条边框=内容 border");
            assert_eq!(chrome.text_primary, colors.fg, "外壳主文字=内容主文字");
            assert_eq!(chrome.text_minor, colors.fg_muted, "外壳次文字=内容次文字");
            assert_eq!(chrome.dirty, colors.error, "脏标记=错误色");
            assert_eq!(colors.surface, colors.popover_bg, "浮动层=升一级表面（surface 同 popover 底）");
        }
        // 明暗双端各自钉死基座色（防回归到旧 GitHub/slate 蓝灰）。
        let dark = ChromeColors::dark();
        assert_eq!(dark.window_bg, rgb(0x202020), "暗窗口背景");
        assert_eq!(ColorScheme::dark().bg, rgb(0x2b2b2b), "暗内容主背景");
        assert_eq!(ColorScheme::light().bg, rgb(0xffffff), "亮内容主背景");
    }

    #[gpui::test]
    fn typography_default_has_mono_family_and_unified_ratio(_cx: &mut TestAppContext) {
        let t = Typography::default();
        assert!(!t.mono.is_empty(), "等宽字体族不应为空");
        assert_eq!(t.mono, Typography::default().mono, "字体族跨明暗不变");
        // 行高比例保留 gpui-component mono_md 的 20/13（编辑器缩放无回归）。
        assert!((t.editor_line_height_ratio - 20.0 / 13.0).abs() < 1e-6);
        // 编辑器无覆盖默认保留 mono_md 的 13（应用侧恒有 override，仅供无覆盖/测试路径）。
        assert_eq!(t.editor_default_size, 13.0);
        // 缺省正文字号＝统一缩放架构的全局驱动值。
        assert_eq!(t.default_body_size, config::layout::DEFAULT_FONT_SIZE);
        assert_eq!(
            (t.editor_default_size * t.editor_line_height_ratio).round(),
            20.0,
            "编辑器默认行高保留 20"
        );
    }

    #[gpui::test]
    fn language_badge_colors_are_vivid_distinct_and_adapt_to_mode(_cx: &mut TestAppContext) {
        // P2-11：状态栏语言徽标的「徽记色点」——不同语言必须可辨、同语言深浅两套亮度
        // 拉开、纯文本类降饱和为中性（与语言类拉开层次）。
        let rust_l = language_badge_color("Rust", false);
        let rust_d = language_badge_color("Rust", true);
        let md_d = language_badge_color("Markdown", true);
        let txt_d = language_badge_color("纯文本", true);
        assert_ne!(rust_l, rust_d, "同语言深浅底色亮度应不同");
        assert_ne!(rust_d, md_d, "不同语言（Rust 橙 vs Markdown 蓝）应可辨");
        assert_ne!(rust_d.h, md_d.h, "色相应不同");
        assert!(
            txt_d.s < rust_d.s - 0.2,
            "纯文本类徽点应显著降饱和（中性灰蓝，s={} vs Rust s={}）",
            txt_d.s,
            rust_d.s
        );
        assert!(
            (rust_l.l - rust_d.l).abs() > 0.2,
            "亮/暗档亮度差应明显（brightness 可辨）"
        );
        // 未知语言回退到中性，绝不等同某语言辨识色。
        let unknown_d = language_badge_color("文本", true);
        assert!(unknown_d.s < 0.3, "未知类别应低饱和中性");
    }
}
