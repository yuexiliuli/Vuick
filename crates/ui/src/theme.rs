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

use std::collections::BTreeMap;
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
#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// 卡片/弹层投影底色（`card_shell` 阴影合成基底；亮暗一致黑色系，各层 alpha 由
    /// `FLOAT_SHADOW_*_ALPHA` / `SINGLE_SHADOW_ALPHA` 给出，消费方经 `card_shell` 只读）
    pub card_shadow: Rgba,
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
    /// 缩进引导线（indentation guides）**非 active** 竖线色 = VSCode 默认灰（Light+
    /// #D3D3D3 / Dark+ #404040，2026-11-14「完全对齐 VSCode 默认主题」；替代旧
    /// `fg_muted.opacity(0.4)` 淡灰）。
    pub guide_inactive: Rgba,
    /// 缩进引导线 **active**（光标所在纯缩进组）点亮竖线色 = VSCode 默认灰（Light+
    /// #939393 / Dark+ #707070；替代旧 `accent.opacity(0.9)` 强调蓝；`active_line_marker_overlay`
    /// 顶部 L 形横线已删，仅剩竖线灰系高亮）。
    pub guide_active: Rgba,
    /// 括号引导线（bracket guides）**3 色循环池** = VSCode `editorBracketGuide.background`
    /// 取色（Light+：#0431FA 蓝 / #319331 绿 / #7B3814 棕；Dark+：#FFD700 金 / #DA70D6
    /// 紫 / #179FFF 青，按序循环）。消费方按 `depth % 3` 取色，与缩进引导线的灰系竖线
    /// 在视觉上区分「结构缩进」与「括号配对」两条线系。
    pub bracket_guide_palette: [Rgba; 3],
    /// **文档选区底色**（半透明补偿蓝叠加，markdown 预览 / WYSIWYG / PDF 阅读器共用；
    /// 暗 `rgba(0x4d6bfe61)` / 亮 `rgba(0x4d6bfe42)` = 各消费方收编前原值，零观感变化）。
    /// 与 `editor_selection_bg`（代码编辑器选区补偿色）语义区分：本 token 管**文档预览/纸面**
    /// 上的文本选区。
    pub doc_selection_bg: Rgba,
    // ── 表面对坡 / 强调派生 / 状态色（全站语义色主题系统 · ui 模型层，2026-11-22）──
    // default 预设的缺省值即「零覆盖 = 现状逐字节不变」的基座：本批新增字段只增加能力，
    // 不改既有字段任何观感；预设/定制经 `Theme::apply` 合并注入（见 `merge_spectrum`）。
    /// 表面坡最深层（窗口底 / 最深面板；暗档中性灰 #121314）
    pub srf_deep: Rgba,
    /// 表面坡主层（主体表面背景；暗档 #181a22）
    pub srf_primary: Rgba,
    /// 表面坡次层（略抬升的表面；暗档 #1d1f26）
    pub srf_secondary: Rgba,
    /// 表面坡卡片层（卡片/弹层抬升面；暗档 #22242e）
    pub srf_card: Rgba,
    /// 表面坡悬停层（悬浮于卡片之上的表面；暗档 #282a34）
    pub srf_hover: Rgba,
    /// 表面分隔线（浮于表面坡上的分隔；暗档 白×6% ≈ rgba(255,255,255,0.06)）
    pub srf_border: Rgba,
    /// 强调色提亮一档（派生自 `accent` 同色相提亮度——default 档 hardcode 近似值，
    /// 预设表须显式列派生色，见 `PRESETS`）
    pub accent_light: Rgba,
    /// 覆盖层用的纯白（遮罩底/模糊底，亮暗同值）
    pub overlay_white: Rgba,
    /// 成功态语义色（Fluent 绿系：亮档深一档 #107c10 系 / 暗档亮一档，对齐 `error` 模式）
    pub success: Rgba,
    /// 警告态语义色（Fluent 橙系）
    pub warning: Rgba,
    /// 信息态语义色（Fluent 蓝系）
    pub info: Rgba,
    /// 收藏/喜爱态语义色（Fluent 粉系）
    pub favorite: Rgba,
}

impl ColorScheme {
    /// 按明暗模式取对应一套（便捷入口，等价 [`ColorScheme::light`]/[`ColorScheme::dark`]）。
    pub fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }
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
            // 卡片投影底色 = 不透明黑（原 float_card 裸 `hsla(0,0,0,α)` 收编；明暗一致）。
            card_shadow: rgb(0x000000),
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
            // VSCode Light+ 默认缩进引导线（灰系；与 active 档 #939393 区分）。
            guide_inactive: rgb(0xd3d3d3),
            guide_active: rgb(0x939393),
            // VSCode Light+ 括号引导线 3 色循环池（#0431FA / #319331 / #7B3814）。
            bracket_guide_palette: [rgb(0x0431fa), rgb(0x319331), rgb(0x7b3814)],
            doc_selection_bg: rgba(0x4d6bfe42),
            // 表面对坡（亮档浅灰坡，中性灰、非紫——紫色由预设注入）。
            srf_deep: rgb(0xf4f5f7),
            srf_primary: rgb(0xeceef1),
            srf_secondary: rgb(0xe5e8ed),
            srf_card: rgb(0xdfe3e9),
            srf_hover: rgb(0xd9dde4),
            // 表面分隔线 = 黑×6%（≈ rgba(0,0,0,0.06)）。
            srf_border: rgba(0x0000000f),
            // 强调提亮一档（派生自 accent #005FB8，brightness 抬升近似；预设表显式列派生色）。
            accent_light: rgb(0x2a7fd6),
            // 覆盖层纯白（亮暗同值）。
            overlay_white: rgb(0xffffff),
            // Fluent 状态语义色：亮档深一档（对齐 error 亮档 #C42B1C 模式）。
            success: rgb(0x107c10),
            warning: rgb(0x9d5d00),
            info: rgb(0x0f6cbd),
            favorite: rgb(0xd63384),
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
            // 卡片投影底色 = 不透明黑（同亮档，原 float_card 裸 `hsla(0,0,0,α)` 收编）。
            card_shadow: rgb(0x000000),
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
            // VSCode Dark+ 默认缩进引导线（灰系；与 active 档 #707070 区分）。
            guide_inactive: rgb(0x404040),
            guide_active: rgb(0x707070),
            // VSCode Dark+ 括号引导线 3 色循环池（#FFD700 / #DA70D6 / #179FFF）。
            bracket_guide_palette: [rgb(0xffd700), rgb(0xda70d6), rgb(0x179fff)],
            doc_selection_bg: rgba(0x4d6bfe61),
            // 表面对坡（暗档中性灰坡，非紫——紫色由预设注入）。
            srf_deep: rgb(0x121314),
            srf_primary: rgb(0x181a22),
            srf_secondary: rgb(0x1d1f26),
            srf_card: rgb(0x22242e),
            srf_hover: rgb(0x282a34),
            // 表面分隔线 = 白×6%（≈ rgba(255,255,255,0.06)）。
            srf_border: rgba(0xffffff0f),
            // 强调提亮一档（派生自 accent #60CDFF，brightness 抬升近似；预设表显式列派生色）。
            accent_light: rgb(0xa0e2ff),
            // 覆盖层纯白（亮暗同值）。
            overlay_white: rgb(0xffffff),
            // Fluent 状态语义色：暗档亮一档（对齐 error 暗档 #F16A6A 模式）。
            success: rgb(0x6ccb5f),
            warning: rgb(0xffb900),
            info: rgb(0x4cc2ff),
            favorite: rgb(0xf57eb6),
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
#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// 选项卡拖拽排序「插入指示线」纯白（亮暗同值；原 shell_tabs 裸 `gpui::white()`，
    /// 与外壳底再暗/再亮都保持白线对比）
    pub tab_reorder_line: Rgba,
}

/// 自绘玻璃材质 chrome 透明度：标题栏/选项卡栏/侧栏/状态栏/窗口底用
/// （半透明才能显出应用内自绘材质层，同时保留文字对比。不依赖系统 Mica/acrylic）。
pub const MICA_CHROME_ALPHA_LIGHT: f32 = 0.86;
pub const MICA_CHROME_ALPHA_DARK: f32 = 0.82;
/// 浮动层（菜单/通知等）自绘玻璃材质透明度（不依赖系统 acrylic）。
pub const FLOAT_ACRYLIC_ALPHA_LIGHT: f32 = 0.92;
pub const FLOAT_ACRYLIC_ALPHA_DARK: f32 = 0.94;
/// 浮动卡片双层投影高层 α（palette 现值：(0,8) blur14，α0.20）。
pub const FLOAT_SHADOW_HIGH_ALPHA: f32 = 0.20;
/// 浮动卡片双层投影底层 α（palette 现值：(0,2) blur4，α0.14）。
pub const FLOAT_SHADOW_PEAK_ALPHA: f32 = 0.14;
/// 单影档 α（video hover_time 现值：(0,2) blur6，α0.16；B02b glass_card 预留）。
pub const SINGLE_SHADOW_ALPHA: f32 = 0.16;
/// B02b 玻璃面板默认 α（亮档）：`colors.surface` 玻璃底现值（video placeholder /
/// hover_time / 状态浮层共用；半透明才能透出视频画面，同时保留文字对比）。
/// 原散值在 video/viewer.rs `GLASS_PANEL_ALPHA_*`，随 glass_card 收编进 theme。
pub const GLASS_PANEL_ALPHA_LIGHT: f32 = 0.80;
/// B02b 玻璃面板默认 α（暗档）：同 [`GLASS_PANEL_ALPHA_LIGHT`]，暗主题深玻璃。
pub const GLASS_PANEL_ALPHA_DARK: f32 = 0.72;
/// 玻璃卡双层软影外层 α（video placeholder 现值：(0,4) blur12，α0.16）。
pub const GLASS_DOUBLE_HIGH_ALPHA: f32 = 0.16;
/// 玻璃卡/选区弹卡双层影近层 α（video (0,1) blur3 / pdf (0,2) blur4 共用 α0.12）。
pub const GLASS_DOUBLE_PEAK_ALPHA: f32 = 0.12;
/// 选区弹卡双层影外层 α（pdf sel_card 现值：(0,6) blur12，α0.18）。
pub const POPOVER_DOUBLE_HIGH_ALPHA: f32 = 0.18;
/// 状态浮层双层影外层 α（video status toast 现值：(0,4) blur10，α0.18；
/// 与 PopoverDouble 外层同 α、偏移/模糊不同档，故独立成档）。
pub const TOAST_DOUBLE_HIGH_ALPHA: f32 = 0.18;
/// 状态浮层双层影近层 α（video status toast 现值：(0,1) blur2，α0.14；
/// 与 NearDouble/PopoverDouble 的近层 α0.12 不同，故独立成档）。
pub const TOAST_DOUBLE_PEAK_ALPHA: f32 = 0.14;

/// 由 RRGGBBAA 十六进制编译期构造 [`Rgba`]（`gpui::rgba` 非 const fn，这里用结构体
/// 字面量等价：逐分量 `byte as f32 / 255.0`，与 `rgba(hex)` 逐位一致）。
const fn const_rgba(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 24) & 0xff) as f32 / 255.0,
        g: ((hex >> 16) & 0xff) as f32 / 255.0,
        b: ((hex >> 8) & 0xff) as f32 / 255.0,
        a: (hex & 0xff) as f32 / 255.0,
    }
}

/// 由 RRGGBB 十六进制编译期构造不透明 [`Rgba`]（同 `gpui::rgb`：忽略高字节、alpha 恒 1）。
const fn const_rgb(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// 通用悬停填充（亮档，黑 5% `0x0000000d`）：`ChromeColors::light().hover_fill` 单一来源。
pub const CHROME_HOVER_FILL_LIGHT: Rgba = const_rgba(0x0000000d);
/// 通用悬停填充（暗档，白 8% `0xffffff14`）：`ChromeColors::dark().hover_fill` 单一来源；
/// music 窗口控制钮 hover 填充逐字对齐本值。
pub const CHROME_HOVER_FILL_DARK: Rgba = const_rgba(0xffffff14);
/// 通用按压填充（亮档，黑 9% `0x00000017`）：`ChromeColors::light().pressed_fill` 单一来源。
pub const CHROME_PRESSED_FILL_LIGHT: Rgba = const_rgba(0x00000017);
/// 通用按压填充（暗档，白 14% `0xffffff24`）：`ChromeColors::dark().pressed_fill` 单一来源；
/// music 窗口控制钮按压填充逐字对齐本值。
pub const CHROME_PRESSED_FILL_DARK: Rgba = const_rgba(0xffffff24);
/// 关闭按钮悬停填充（Win11 危险红 `#e81123`，亮暗同值）。
pub const CHROME_CLOSE_HOVER: Rgba = const_rgb(0xe81123);
/// 关闭按钮按压填充（危险红深一档 `#a70d1a`，亮暗同值）。
pub const CHROME_CLOSE_PRESSED: Rgba = const_rgb(0xa70d1a);

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
            hover_fill: CHROME_HOVER_FILL_LIGHT,
            pressed_fill: CHROME_PRESSED_FILL_LIGHT,
            close_hover: CHROME_CLOSE_HOVER,
            close_pressed: CHROME_CLOSE_PRESSED,
            drag_ghost_bg: c.accent,
            menu_highlight: rgba(0x00000014),
            // 滚动条滑块 = 半透明中性灰（亮暗同值，原 shell rgba(0x6060604a)）
            scrollbar_thumb: rgba(0x6060604a),
            // 选项卡拖拽排序插入指示线 = 纯白（亮暗同值，原 shell_tabs gpui::white()）
            tab_reorder_line: rgb(0xffffff),
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
            hover_fill: CHROME_HOVER_FILL_DARK,
            pressed_fill: CHROME_PRESSED_FILL_DARK,
            close_hover: CHROME_CLOSE_HOVER,
            close_pressed: CHROME_CLOSE_PRESSED,
            drag_ghost_bg: c.accent,
            menu_highlight: rgba(0xffffff1a),
            // 滚动条滑块 = 半透明中性灰（亮暗同值，原 shell rgba(0x6060604a)）
            scrollbar_thumb: rgba(0x6060604a),
            // 选项卡拖拽排序插入指示线 = 纯白（亮暗同值，原 shell_tabs gpui::white()）
            tab_reorder_line: rgb(0xffffff),
        }
    }

    /// 按明暗模式取对应一套（无 App 上下文时的便捷入口）。
    pub fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// 从（可能已合并的）`ColorScheme` 重建外壳缺省：chrome 域内**同名字段**（`panel_bg`/
    /// `bar_border`/`text_primary`/`text_minor`/`chip_text`/`tab_active_bg`/
    /// `tab_active_border`/`pill_fg`/`dirty`）与 `drag_ghost_bg←accent` 直接引用 scheme
    /// 对应字段（保持「同名字段物理同值、永不漂移」不变式，见
    /// `chrome_and_color_scheme_share_ramp`）；其余外壳专属字段（`window_bg`/材质渐变、
    /// `editor_bg`/`chrome_bg`/悬停按压等）按明暗档取 [`ChromeColors::light`]/[`ChromeColors::dark`]
    /// 既有常量。纯函数，供 [`Theme::apply`] 在合并 `colors` 之后重建 chrome（杜绝派生
    /// 漂移），也可供测试；对默认 scheme 与 `for_mode` **逐字段字节相等**（零回归锚）。
    pub fn from_scheme(scheme: &ColorScheme, mode: ThemeMode) -> Self {
        let chrome = if matches!(mode, ThemeMode::Dark) { Self::dark() } else { Self::light() };
        Self {
            window_bg: chrome.window_bg,
            material_top: chrome.material_top,
            material_bottom: chrome.material_bottom,
            bar_bg: chrome.bar_bg,
            bar_border: scheme.border,
            hairline: chrome.hairline,
            panel_bg: scheme.bg,
            editor_bg: chrome.editor_bg,
            chrome_bg: chrome.chrome_bg,
            text_primary: scheme.fg,
            text_minor: scheme.fg_muted,
            text_tertiary: chrome.text_tertiary,
            chip_bg: chrome.chip_bg,
            chip_text: scheme.fg,
            tab_active_bg: scheme.bg,
            tab_active_border: scheme.border,
            pill_bg: chrome.pill_bg,
            pill_fg: scheme.fg,
            dirty: scheme.error,
            hover_fill: chrome.hover_fill,
            pressed_fill: chrome.pressed_fill,
            close_hover: chrome.close_hover,
            close_pressed: chrome.close_pressed,
            drag_ghost_bg: scheme.accent,
            menu_highlight: chrome.menu_highlight,
            scrollbar_thumb: chrome.scrollbar_thumb,
            tab_reorder_line: chrome.tab_reorder_line,
        }
    }
}

/// 主题令牌：`ColorScheme` + `ChromeColors` 全字段的文本寻址（`"colors.xxx"` /
/// `"chrome.xxx"`）。
///
/// - [`ColorToken::key`]：字段 → 键（如 `"colors.accent"` / `"chrome.window_bg"`）。
/// - [`ColorToken::from_key`]：键 → 令牌；未知键返回 `None`（调用方兜底，不 panic）。
/// - [`ColorToken::parse_hex`]：仅接受 `#rrggbb` 6 位十六进制（此处先自立实现、复用
///   config 校验语义，config slice 再联动）。
///
/// 覆盖「全部字段」含数组板（`mermaid_accent_palette`/`bracket_guide_palette`），但数组板
/// 不可被单个 hex 覆盖，合并时跳过（[`apply_color_token`]/[`apply_chrome_token`] no-op）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorToken {
    // ── ColorScheme（colors.*）──
    /// 主背景（画布/正文底色）
    Bg,
    /// 面板/状态栏等次级表面
    Surface,
    /// 主文字
    Fg,
    /// 次要文字
    FgMuted,
    /// 分隔线/边框
    Border,
    /// 强调色
    Accent,
    /// 链接
    Link,
    /// 错误/危险色
    Error,
    /// 禁用态弱化文字
    FgDisabled,
    /// 聚焦控件描边
    FocusRing,
    /// 选中/悬停基底
    SelectedBg,
    /// 按压/激活基底
    PressedBg,
    /// 拖拽/悬停强调线
    DragHoverBorder,
    /// 卡片/弹层投影底色
    CardShadow,
    /// 代码块背景
    CodeBlockBg,
    /// 行内代码文字
    CodeInlineFg,
    /// 行内代码 pill 底
    CodeInlineBg,
    /// 编辑器滤镜行内代码底
    EditorCodeBg,
    /// 引述块背景
    QuoteBg,
    /// 表头底色
    TableHeadBg,
    /// Markdown 标题文字
    Heading,
    /// 悬浮小卡片/弹层背景
    PopoverBg,
    /// Mermaid 节点分类板（数组，不可覆盖）
    MermaidAccentPalette,
    /// Mermaid 边线/边框
    MermaidBorder,
    /// 编辑器选区底（半透明补偿色）
    EditorSelectionBg,
    /// 编辑器折叠行底
    EditorFoldBg,
    /// 编辑器光标行底
    EditorActiveLineBg,
    /// 编辑器查找非当前命中底
    EditorFindMatchBg,
    /// 编辑器查找当前命中底
    EditorFindCurrentBg,
    /// 缩进引导线非 active 竖线
    GuideInactive,
    /// 缩进引导线 active 竖线
    GuideActive,
    /// 括号引导线 3 色池（数组，不可覆盖）
    BracketGuidePalette,
    /// 文档选区底（半透明补偿蓝）
    DocSelectionBg,
    // ── 表面对坡 / 强调派生 / 状态色（全站语义色主题系统新增）──
    /// 表面坡最深层
    SrfDeep,
    /// 表面坡主层
    SrfPrimary,
    /// 表面坡次层
    SrfSecondary,
    /// 表面坡卡片层
    SrfCard,
    /// 表面坡悬停层
    SrfHover,
    /// 表面分隔线
    SrfBorder,
    /// 强调色提亮一档
    AccentLight,
    /// 覆盖层纯白
    OverlayWhite,
    /// 成功态语义色
    Success,
    /// 警告态语义色
    Warning,
    /// 信息态语义色
    Info,
    /// 收藏/喜爱态语义色
    Favorite,
    // ── ChromeColors（chrome.*）──
    /// 窗口背景
    WindowBg,
    /// 自绘材质渐变顶端
    MaterialTop,
    /// 自绘材质渐变底端
    MaterialBottom,
    /// 工具条背景
    BarBg,
    /// 工具条分隔边框
    BarBorder,
    /// 区域分隔发丝线
    Hairline,
    /// 面板背景
    PanelBg,
    /// 内容区真实渲染底
    EditorBg,
    /// 外壳 chrome 区域底色
    ChromeBg,
    /// 主要文字
    TextPrimary,
    /// 次要文字
    TextMinor,
    /// 三级文字
    TextTertiary,
    /// 芯片背景
    ChipBg,
    /// 芯片文字
    ChipText,
    /// 激活选项卡背景
    TabActiveBg,
    /// 激活选项卡边框
    TabActiveBorder,
    /// pill 底色
    PillBg,
    /// pill 前景文字
    PillFg,
    /// 文件脏标记点
    Dirty,
    /// 通用悬停填充
    HoverFill,
    /// 通用按压填充
    PressedFill,
    /// 关闭按钮悬停填充
    CloseHover,
    /// 关闭按钮按压填充
    ClosePressed,
    /// 选项卡拖拽幽灵胶囊底
    DragGhostBg,
    /// 菜单项选中/悬停高亮
    MenuHighlight,
    /// 横向滚动条滑块
    ScrollbarThumb,
    /// 选项卡拖拽插入指示线
    TabReorderLine,
}

/// 由 `$(variant => key)` 表一次性生成 `key()`/`from_key()`（单一事实源，防两表漂移）。
macro_rules! color_token_keys {
    ($($variant:ident => $key:literal),* $(,)?) => {
        /// 令牌 → 文本键（如 `"colors.accent"`）。
        pub fn key(self) -> &'static str {
            match self {
                $( Self::$variant => $key, )*
            }
        }
        /// 文本键 → 令牌；未知返回 `None`。
        pub fn from_key(key: &str) -> Option<Self> {
            Some(match key {
                $( $key => Self::$variant, )*
                _ => return None,
            })
        }
    };
}

impl ColorToken {
    color_token_keys! {
        Bg => "colors.bg",
        Surface => "colors.surface",
        Fg => "colors.fg",
        FgMuted => "colors.fg_muted",
        Border => "colors.border",
        Accent => "colors.accent",
        Link => "colors.link",
        Error => "colors.error",
        FgDisabled => "colors.fg_disabled",
        FocusRing => "colors.focus_ring",
        SelectedBg => "colors.selected_bg",
        PressedBg => "colors.pressed_bg",
        DragHoverBorder => "colors.drag_hover_border",
        CardShadow => "colors.card_shadow",
        CodeBlockBg => "colors.code_block_bg",
        CodeInlineFg => "colors.code_inline_fg",
        CodeInlineBg => "colors.code_inline_bg",
        EditorCodeBg => "colors.editor_code_bg",
        QuoteBg => "colors.quote_bg",
        TableHeadBg => "colors.table_head_bg",
        Heading => "colors.heading",
        PopoverBg => "colors.popover_bg",
        MermaidAccentPalette => "colors.mermaid_accent_palette",
        MermaidBorder => "colors.mermaid_border",
        EditorSelectionBg => "colors.editor_selection_bg",
        EditorFoldBg => "colors.editor_fold_bg",
        EditorActiveLineBg => "colors.editor_active_line_bg",
        EditorFindMatchBg => "colors.editor_find_match_bg",
        EditorFindCurrentBg => "colors.editor_find_current_bg",
        GuideInactive => "colors.guide_inactive",
        GuideActive => "colors.guide_active",
        BracketGuidePalette => "colors.bracket_guide_palette",
        DocSelectionBg => "colors.doc_selection_bg",
        SrfDeep => "colors.srf_deep",
        SrfPrimary => "colors.srf_primary",
        SrfSecondary => "colors.srf_secondary",
        SrfCard => "colors.srf_card",
        SrfHover => "colors.srf_hover",
        SrfBorder => "colors.srf_border",
        AccentLight => "colors.accent_light",
        OverlayWhite => "colors.overlay_white",
        Success => "colors.success",
        Warning => "colors.warning",
        Info => "colors.info",
        Favorite => "colors.favorite",
        WindowBg => "chrome.window_bg",
        MaterialTop => "chrome.material_top",
        MaterialBottom => "chrome.material_bottom",
        BarBg => "chrome.bar_bg",
        BarBorder => "chrome.bar_border",
        Hairline => "chrome.hairline",
        PanelBg => "chrome.panel_bg",
        EditorBg => "chrome.editor_bg",
        ChromeBg => "chrome.chrome_bg",
        TextPrimary => "chrome.text_primary",
        TextMinor => "chrome.text_minor",
        TextTertiary => "chrome.text_tertiary",
        ChipBg => "chrome.chip_bg",
        ChipText => "chrome.chip_text",
        TabActiveBg => "chrome.tab_active_bg",
        TabActiveBorder => "chrome.tab_active_border",
        PillBg => "chrome.pill_bg",
        PillFg => "chrome.pill_fg",
        Dirty => "chrome.dirty",
        HoverFill => "chrome.hover_fill",
        PressedFill => "chrome.pressed_fill",
        CloseHover => "chrome.close_hover",
        ClosePressed => "chrome.close_pressed",
        DragGhostBg => "chrome.drag_ghost_bg",
        MenuHighlight => "chrome.menu_highlight",
        ScrollbarThumb => "chrome.scrollbar_thumb",
        TabReorderLine => "chrome.tab_reorder_line",
    }

    /// 解析 `#rrggbb`（6 位，不透明 → alpha=1.0）或 `#rrggbbaa`（8 位，保留 alpha）十六进制
    /// → [`Rgba`]；缺 `#` / 长度非 6/8 位 / 非法十六进制一律返回 `None`
    /// （调用方跳过该 key，不 panic）。8 位支持让 `srf_border` 等半透明人格令牌诚实落位
    /// （P2-2：`"#ffffff0f"` 此前因 8 位被跳过 → border 恒 base）。
    pub fn parse_hex(s: &str) -> Option<Rgba> {
        let b = s.as_bytes();
        if b.len() != 7 && b.len() != 9 {
            return None;
        }
        if b[0] != b'#' {
            return None;
        }
        match b.len() {
            7 => {
                // #rrggbb：r/g/b = 高 3 字节、alpha 恒 1.0（同 `gpui::rgb`）。
                let mut val: u32 = 0;
                for &c in &b[1..] {
                    val <<= 4;
                    val |= (c as char).to_digit(16)?;
                }
                Some(rgb(val))
            }
            _ => {
                // #rrggbbaa：逐字节解析，alpha 位保留（同 `gpui::rgba` 的 0xrrggbbaa 布局）。
                let byte = |i: usize| -> Option<u8> {
                    let hi = (b[i] as char).to_digit(16)?;
                    let lo = (b[i + 1] as char).to_digit(16)?;
                    Some((hi * 16 + lo) as u8)
                };
                let (r, g, bl, a) = (byte(1)?, byte(3)?, byte(5)?, byte(7)?);
                Some(rgba(
                    ((r as u32) << 24) | ((g as u32) << 16) | ((bl as u32) << 8) | a as u32,
                ))
            }
        }
    }
}

/// UI 可暴露子集：config/设置面板允许用户覆写的**人格面 + 基础文字/表面**令牌（防把全
/// 72 枚令牌面全暴露成 over-engineering；chrome.* / 代码 / Mermaid 等专业令牌不进用户面）。
pub const UI_EXPOSABLE_TOKENS: &[ColorToken] = &[
    ColorToken::Accent,
    ColorToken::AccentLight,
    ColorToken::SrfDeep,
    ColorToken::SrfPrimary,
    ColorToken::SrfSecondary,
    ColorToken::SrfCard,
    ColorToken::SrfHover,
    ColorToken::SrfBorder,
    ColorToken::Bg,
    ColorToken::Surface,
    ColorToken::Fg,
    ColorToken::FgMuted,
    ColorToken::FgDisabled,
    ColorToken::Border,
    ColorToken::Success,
    ColorToken::Warning,
    ColorToken::Info,
    ColorToken::Favorite,
    ColorToken::OverlayWhite,
];

/// 把十六进制解析值写入 `ColorScheme` 对应字段；本域外令牌（chrome.* / 数组板）为 no-op。
fn apply_color_token(cs: &mut ColorScheme, token: ColorToken, v: Rgba) {
    match token {
        ColorToken::Bg => cs.bg = v,
        ColorToken::Surface => cs.surface = v,
        ColorToken::Fg => cs.fg = v,
        ColorToken::FgMuted => cs.fg_muted = v,
        ColorToken::Border => cs.border = v,
        ColorToken::Accent => cs.accent = v,
        ColorToken::Link => cs.link = v,
        ColorToken::Error => cs.error = v,
        ColorToken::FgDisabled => cs.fg_disabled = v,
        ColorToken::FocusRing => cs.focus_ring = v,
        ColorToken::SelectedBg => cs.selected_bg = v,
        ColorToken::PressedBg => cs.pressed_bg = v,
        ColorToken::DragHoverBorder => cs.drag_hover_border = v,
        ColorToken::CardShadow => cs.card_shadow = v,
        ColorToken::CodeBlockBg => cs.code_block_bg = v,
        ColorToken::CodeInlineFg => cs.code_inline_fg = v,
        ColorToken::CodeInlineBg => cs.code_inline_bg = v,
        ColorToken::EditorCodeBg => cs.editor_code_bg = v,
        ColorToken::QuoteBg => cs.quote_bg = v,
        ColorToken::TableHeadBg => cs.table_head_bg = v,
        ColorToken::Heading => cs.heading = v,
        ColorToken::PopoverBg => cs.popover_bg = v,
        ColorToken::MermaidAccentPalette => {}
        ColorToken::MermaidBorder => cs.mermaid_border = v,
        ColorToken::EditorSelectionBg => cs.editor_selection_bg = v,
        ColorToken::EditorFoldBg => cs.editor_fold_bg = v,
        ColorToken::EditorActiveLineBg => cs.editor_active_line_bg = v,
        ColorToken::EditorFindMatchBg => cs.editor_find_match_bg = v,
        ColorToken::EditorFindCurrentBg => cs.editor_find_current_bg = v,
        ColorToken::GuideInactive => cs.guide_inactive = v,
        ColorToken::GuideActive => cs.guide_active = v,
        ColorToken::BracketGuidePalette => {}
        ColorToken::DocSelectionBg => cs.doc_selection_bg = v,
        ColorToken::SrfDeep => cs.srf_deep = v,
        ColorToken::SrfPrimary => cs.srf_primary = v,
        ColorToken::SrfSecondary => cs.srf_secondary = v,
        ColorToken::SrfCard => cs.srf_card = v,
        ColorToken::SrfHover => cs.srf_hover = v,
        ColorToken::SrfBorder => cs.srf_border = v,
        ColorToken::AccentLight => cs.accent_light = v,
        ColorToken::OverlayWhite => cs.overlay_white = v,
        ColorToken::Success => cs.success = v,
        ColorToken::Warning => cs.warning = v,
        ColorToken::Info => cs.info = v,
        ColorToken::Favorite => cs.favorite = v,
        _ => {} // chrome.* / 数组板以外的颜色令牌不存在；本域仅颜色令牌，其余不作用
    }
}

/// 把十六进制解析值写入 `ChromeColors` 对应字段；本域外令牌（colors.* / 数组板）为 no-op。
fn apply_chrome_token(cc: &mut ChromeColors, token: ColorToken, v: Rgba) {
    match token {
        ColorToken::WindowBg => cc.window_bg = v,
        ColorToken::MaterialTop => cc.material_top = v,
        ColorToken::MaterialBottom => cc.material_bottom = v,
        ColorToken::BarBg => cc.bar_bg = v,
        ColorToken::BarBorder => cc.bar_border = v,
        ColorToken::Hairline => cc.hairline = v,
        ColorToken::PanelBg => cc.panel_bg = v,
        ColorToken::EditorBg => cc.editor_bg = v,
        ColorToken::ChromeBg => cc.chrome_bg = v,
        ColorToken::TextPrimary => cc.text_primary = v,
        ColorToken::TextMinor => cc.text_minor = v,
        ColorToken::TextTertiary => cc.text_tertiary = v,
        ColorToken::ChipBg => cc.chip_bg = v,
        ColorToken::ChipText => cc.chip_text = v,
        ColorToken::TabActiveBg => cc.tab_active_bg = v,
        ColorToken::TabActiveBorder => cc.tab_active_border = v,
        ColorToken::PillBg => cc.pill_bg = v,
        ColorToken::PillFg => cc.pill_fg = v,
        ColorToken::Dirty => cc.dirty = v,
        ColorToken::HoverFill => cc.hover_fill = v,
        ColorToken::PressedFill => cc.pressed_fill = v,
        ColorToken::CloseHover => cc.close_hover = v,
        ColorToken::ClosePressed => cc.close_pressed = v,
        ColorToken::DragGhostBg => cc.drag_ghost_bg = v,
        ColorToken::MenuHighlight => cc.menu_highlight = v,
        ColorToken::ScrollbarThumb => cc.scrollbar_thumb = v,
        ColorToken::TabReorderLine => cc.tab_reorder_line = v,
        _ => {} // colors.* / 数组板：本域不作用
    }
}

/// 表驱动预设：每预设只列**人格令牌**（accent/accent_light/srf_*/状态色偏差，6-10 条）；
/// base 恒为 [`ColorScheme::light`]/[`ColorScheme::dark`]（零列 = 该档零覆盖 = 现状逐字节
/// 不变）。`light`/`dark` 两表互不污染，由 [`ThemeSpec::mode`] 择一合并。
#[derive(Clone, Copy, Debug)]
pub struct PresetSkin {
    /// 稳定标识（config 传 id 用，如 `"music"`）。
    pub id: &'static str,
    /// 用户可见名（如 `"音乐紫"`）。
    pub name: &'static str,
    /// 亮档人格令牌覆盖表（`(ColorToken, "#rrggbb")`）
    pub light: &'static [(ColorToken, &'static str)],
    /// 暗档人格令牌覆盖表（`(ColorToken, "#rrggbb")`）
    pub dark: &'static [(ColorToken, &'static str)],
}

/// 内置预设集合。首项必为 default（中性，空覆盖）——`Theme::set` 薄包装以它为兜底。
pub static PRESETS: &[PresetSkin] = &[
    PresetSkin {
        id: "default",
        name: "中性",
        light: &[],
        dark: &[],
    },
    PresetSkin {
        id: "music",
        name: "音乐紫",
        // 暗档：紫调深坡 + 紫 accent（#7C5CFC / 提亮 #A78BFA）。
        dark: &[
            (ColorToken::Accent, "#7c5cfc"),
            (ColorToken::AccentLight, "#a78bfa"),
            (ColorToken::SrfDeep, "#0d0b1a"),
            (ColorToken::SrfPrimary, "#13112a"),
            (ColorToken::SrfSecondary, "#1a1738"),
            (ColorToken::SrfCard, "#201d40"),
            (ColorToken::SrfHover, "#282452"),
            (ColorToken::SrfBorder, "#ffffff0f"),
        ],
        // 亮档：协调的紫调浅坡（浅紫灰在亮底上仍中性可读；accent 亮档深一档保对比）。
        light: &[
            (ColorToken::Accent, "#5b3fd1"),
            (ColorToken::AccentLight, "#8468dc"), // 对比度修正 #1：原 #a78bfa 亮面坡对比 1.99-2.52 <3，加深一档仍「音乐紫」
            (ColorToken::SrfDeep, "#f7f5ff"),
            (ColorToken::SrfPrimary, "#efecfc"),
            (ColorToken::SrfSecondary, "#e7e2f8"),
            (ColorToken::SrfCard, "#dfd9f4"),
            (ColorToken::SrfHover, "#d7d0f0"),
            (ColorToken::SrfBorder, "#0000000f"),
        ],
    },
    PresetSkin {
        id: "ocean",
        name: "海青",
        // 暗档：冷蓝灰坡 + 海青 teal accent（#2DD4BF）。
        dark: &[
            (ColorToken::Accent, "#2dd4bf"),
            (ColorToken::AccentLight, "#5eead4"),
            (ColorToken::SrfDeep, "#0b111c"),
            (ColorToken::SrfPrimary, "#0e1826"),
            (ColorToken::SrfSecondary, "#122033"),
            (ColorToken::SrfCard, "#15263a"),
            (ColorToken::SrfHover, "#1a2d44"),
            (ColorToken::SrfBorder, "#ffffff0f"),
        ],
        // 亮档：协调的冷浅蓝灰坡；accent 亮档深一档（#0C9185）保白底对比。
        light: &[
            (ColorToken::Accent, "#0c9185"), // 对比度修正 #2：原 #0d9488 vs 卡片面坡 2.98 <3，微深一档
            (ColorToken::AccentLight, "#109185"), // 对比度修正 #3：原 #14b8a6 亮面坡对比 1.98-2.36 <3，加深一档
            (ColorToken::SrfDeep, "#f4faf9"),
            (ColorToken::SrfPrimary, "#e9f5f3"),
            (ColorToken::SrfSecondary, "#dfeff0"),
            (ColorToken::SrfCard, "#d6e9eb"),
            (ColorToken::SrfHover, "#cde3e6"),
            (ColorToken::SrfBorder, "#0000000f"),
        ],
    },
    PresetSkin {
        id: "solar",
        name: "琥珀",
        // 暗档：暖灰坡 + 琥珀 accent（#F59E0B）。
        dark: &[
            (ColorToken::Accent, "#f59e0b"),
            (ColorToken::AccentLight, "#fbbf24"),
            (ColorToken::SrfDeep, "#1a1408"),
            (ColorToken::SrfPrimary, "#221a0a"),
            (ColorToken::SrfSecondary, "#2b2010"),
            (ColorToken::SrfCard, "#332614"),
            (ColorToken::SrfHover, "#3c2d18"),
            (ColorToken::SrfBorder, "#ffffff0f"),
        ],
        // 亮档：协调的暖浅坡；accent 亮档深一档（#B45309）保白底对比。
        light: &[
            (ColorToken::Accent, "#b45309"),
            (ColorToken::AccentLight, "#c26a05"), // 对比度修正 #4：原 #d97706 vs 卡片面坡 2.53 <3，琥珀深一档
            (ColorToken::SrfDeep, "#fdf8f0"),
            (ColorToken::SrfPrimary, "#faf1e3"),
            (ColorToken::SrfSecondary, "#f7ead7"),
            (ColorToken::SrfCard, "#f4e3ca"),
            (ColorToken::SrfHover, "#f0dcbe"),
            (ColorToken::SrfBorder, "#0000000f"),
        ],
    },
    PresetSkin {
        id: "rose",
        name: "玫红",
        // 暗档：暖褐坡 + 玫红 accent（#F472B6）。
        dark: &[
            (ColorToken::Accent, "#f472b6"),
            (ColorToken::AccentLight, "#f9a8d4"),
            (ColorToken::SrfDeep, "#1c0f16"),
            (ColorToken::SrfPrimary, "#241220"),
            (ColorToken::SrfSecondary, "#2d162a"),
            (ColorToken::SrfCard, "#361a33"),
            (ColorToken::SrfHover, "#3f1f3d"),
            (ColorToken::SrfBorder, "#ffffff0f"),
        ],
        // 亮档：协调的暖浅玫坡；accent 亮档深一档（#BE185D）保白底对比。
        light: &[
            (ColorToken::Accent, "#be185d"),
            (ColorToken::AccentLight, "#db2777"),
            (ColorToken::SrfDeep, "#fdf6f9"),
            (ColorToken::SrfPrimary, "#faedf3"),
            (ColorToken::SrfSecondary, "#f7e4ed"),
            (ColorToken::SrfCard, "#f4dbe7"),
            (ColorToken::SrfHover, "#f1d2e1"),
            (ColorToken::SrfBorder, "#0000000f"),
        ],
    },
    PresetSkin {
        id: "graphite",
        name: "石墨",
        // 暗档：全中性纯黑灰坡 + 中性钢灰 accent，状态色压饱和（去 Fluent 高饱和彩）。
        dark: &[
            (ColorToken::Accent, "#c8ccd2"),
            (ColorToken::AccentLight, "#e6e9ee"),
            (ColorToken::SrfDeep, "#101010"),
            (ColorToken::SrfPrimary, "#161616"),
            (ColorToken::SrfSecondary, "#1c1c1c"),
            (ColorToken::SrfCard, "#222222"),
            (ColorToken::SrfHover, "#2a2a2a"),
            (ColorToken::SrfBorder, "#ffffff0f"),
            (ColorToken::Success, "#7fb069"),
            (ColorToken::Warning, "#c9a25a"),
            (ColorToken::Info, "#8296b8"),
            (ColorToken::Favorite, "#c0809b"),
        ],
        // 亮档：同样中性纯灰坡；accent 中性深灰保白底对比；状态色同样压饱和。
        light: &[
            (ColorToken::Accent, "#5a5f66"),
            (ColorToken::AccentLight, "#717880"), // 对比度修正 #5：原 #7a8087 vs 卡片面坡 2.95 <3，中性灰微深
            (ColorToken::SrfDeep, "#f6f7f8"),
            (ColorToken::SrfPrimary, "#edeeef"),
            (ColorToken::SrfSecondary, "#e4e6e8"),
            (ColorToken::SrfCard, "#dcdedf"),
            (ColorToken::SrfHover, "#d3d5d7"),
            (ColorToken::SrfBorder, "#0000000f"),
            (ColorToken::Success, "#4d7c3f"),
            (ColorToken::Warning, "#9a7b2d"),
            (ColorToken::Info, "#5a6f96"),
            (ColorToken::Favorite, "#a05f7f"),
        ],
    },
];

/// 主题规格（纯数据，不依赖 config）：config 只传 `id`/`hex` 字符串，ui 内解析。
///
/// - `mode`：明暗档。
/// - `preset`：人格预设（运行时为准；`PRESETS` 内取，未知 id 由调用方兜底 default）。
/// - `custom`：`"colors.accent" -> "#rrggbb"` 覆写表（`"default"` = 还原到预设值）。
#[derive(Clone, Debug)]
pub struct ThemeSpec {
    pub mode: ThemeMode,
    pub preset: &'static PresetSkin,
    pub custom: BTreeMap<&'static str, String>,
}

impl ThemeSpec {
    /// 纯默认规格（default 预设 + 空覆写）——`Theme::set` 薄包装与调用方兜底用。
    pub fn plain(mode: ThemeMode) -> Self {
        Self {
            mode,
            preset: &PRESETS[0],
            custom: BTreeMap::new(),
        }
    }
}

/// 合并 `ColorScheme`：优先级 `base < preset < custom`；`custom` 值 `"default"`（或不含该
/// key）＝还原到 preset 值。非法 hex / 未知 key 跳过、不 panic。**只在 apply 时跑一次**
/// （性能纪律：Token 是字段、非每帧计算，渲染只读本函数产物）。
pub(crate) fn merge_spectrum(
    mode: ThemeMode,
    base: &ColorScheme,
    preset_skin: &PresetSkin,
    custom: &BTreeMap<&'static str, String>,
) -> ColorScheme {
    let mut out = *base;
    let table = if matches!(mode, ThemeMode::Dark) {
        preset_skin.dark
    } else {
        preset_skin.light
    };
    // 1) base <- preset：整表套用（人格令牌 hex 合法才生效）。
    for &(token, hex) in table {
        if let Some(v) = ColorToken::parse_hex(hex) {
            apply_color_token(&mut out, token, v);
        }
    }
    // 2) preset <- custom：逐个 key 套用。
    for (key, value) in custom {
        let Some(token) = ColorToken::from_key(key) else {
            continue; // 未知 key：跳过
        };
        if value == "default" {
            // 还原到 preset 值（preset 表里该令牌 → 重新套用；不在表里 = 本来就=base，不动）。
            if let Some(&(_, hex)) = table.iter().find(|(t, _)| *t == token) {
                if let Some(v) = ColorToken::parse_hex(hex) {
                    apply_color_token(&mut out, token, v);
                }
            }
            continue;
        }
        if let Some(v) = ColorToken::parse_hex(value) {
            apply_color_token(&mut out, token, v);
        }
        // 非法 hex：跳过该 key，不影响其它 token。
    }
    out
}

/// 合并 `ChromeColors`（外壳层）：仅 custom 的 `chrome.*` 键生效；本 slice 预设不列 chrome
/// 令牌，故不参与。`"default"` 还原到 base 值。非法 hex / 未知 key 跳过、不 panic。
pub(crate) fn merge_chrome(
    base: &ChromeColors,
    custom: &BTreeMap<&'static str, String>,
) -> ChromeColors {
    let mut out = *base;
    for (key, value) in custom {
        let Some(token) = ColorToken::from_key(key) else {
            continue;
        };
        if value == "default" {
            continue; // chrome 无 preset 层，「default」= 还原 base（当前已是）。
        }
        if let Some(v) = ColorToken::parse_hex(value) {
            apply_chrome_token(&mut out, token, v);
        }
    }
    out
}

/// 全局主题：当前模式 + 对应语义色板 + 外壳配色 + 排版参数。
#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: ThemeMode,
    pub colors: ColorScheme,
    pub chrome: ChromeColors,
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
            chrome: ChromeColors::light(),
            typography: Typography::default(),
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: ColorScheme::dark(),
            chrome: ChromeColors::dark(),
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

    /// 应用主题规格：合并 preset + custom → 更新 `mode`/`colors`/`chrome`，写全局 + 刷新
    /// 窗口。**合并只在本次调用跑一次**（性能纪律：合并一次、渲染只读）。
    ///
    /// chrome 必须从「合并后 colors」重建（`ChromeColors::from_scheme`）再叠 `chrome.*`
    /// custom 覆盖——否则 custom/preset 改 `colors.*` 时 chrome 派生字段（`panel_bg`/
    /// `text_primary`/`tab_active_bg`/`pill_fg`/`dirty` 等同名字段）仍是 base 值，违反
    /// 「同名字段物理同值」不变式（P1-1 修派生漂移）。
    pub fn apply(spec: ThemeSpec, cx: &mut App) {
        let base_colors = ColorScheme::for_mode(matches!(spec.mode, ThemeMode::Dark));
        let colors = merge_spectrum(spec.mode, &base_colors, spec.preset, &spec.custom);
        let chrome_base = ChromeColors::from_scheme(&colors, spec.mode);
        let chrome = merge_chrome(&chrome_base, &spec.custom);
        let theme = Self {
            mode: spec.mode,
            colors,
            chrome,
            typography: Typography::default(),
        };
        cx.set_global(theme);
        cx.refresh_windows();
    }

    /// 切换主题（薄包装 = 应用 default 预设 + 空覆写），所有既有调用方零改动。
    pub fn set(mode: ThemeMode, cx: &mut App) {
        Self::apply(ThemeSpec::plain(mode), cx);
    }
}

/// 查看器画布默认底色：亮 #F5F5F5 / 暗 #202020（与外壳 `window_bg` 同源）。
/// 收敛自 image_viewer / video / pdf 三处逐字一致的 `default_canvas`（下一批 P5）。
/// 读**合并后** Theme 的 `chrome.window_bg`（P1-2：不再从 `for_mode` base 现取，
/// 自定义/预设经 apply 合并后此处直接可见）。
pub fn window_bg(cx: &App) -> Hsla {
    Theme::global(cx).chrome.window_bg.into()
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
        "图片" => (0.86, true),       // 品红
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
    fn window_bg_follows_theme_mode(cx: &mut TestAppContext) {
        // 查看器画布默认底色访问器：亮 = 外壳 window_bg 亮档 #F5F5F5、暗 = 暗档 #202020，
        // 与 `default_canvas` 收编前逐字同源（image_viewer / video / pdf 共用）。
        cx.update(|cx| {
            Theme::set(ThemeMode::Light, cx);
            assert_eq!(window_bg(cx), ChromeColors::for_mode(false).window_bg.into());
            Theme::set(ThemeMode::Dark, cx);
            assert_eq!(window_bg(cx), ChromeColors::for_mode(true).window_bg.into());
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
        assert_ne!(
            light.colors.accent, dark.colors.accent,
            "亮/暗强调色不再统一（Fluent 亮深蓝 vs 暗亮蓝）"
        );
        assert_eq!(
            light.colors.accent,
            rgb(0x005fb8),
            "亮主题强调色=#005FB8（Fluent 亮系默认蓝）"
        );
        assert_eq!(
            dark.colors.accent,
            rgb(0x60cdff),
            "暗主题强调色=#60CDFF（Win11 暗色亮蓝）"
        );
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
        assert_ne!(
            light.colors.quote_bg, dark.colors.quote_bg,
            "引述底色随明暗"
        );
        assert_ne!(
            light.colors.table_head_bg, dark.colors.table_head_bg,
            "表头底色随明暗"
        );
        assert_ne!(
            light.colors.quote_bg, light.colors.table_head_bg,
            "亮引述≠亮表头（#F7F7F7 vs #F2F2F2）"
        );
        assert_ne!(
            dark.colors.quote_bg, dark.colors.table_head_bg,
            "暗引述≠暗表头（#262626 vs #323232）"
        );
        // 标题/链接/弹层同源蓝系，随明暗切换
        assert_eq!(light.colors.heading, light.colors.link, "亮标题=亮链接同蓝");
        assert_eq!(dark.colors.heading, dark.colors.link, "暗标题=暗链接同蓝");
        assert_ne!(light.colors.heading, dark.colors.heading, "标题色随明暗");
        // 悬浮卡片底随明暗（亮 #FFFFFF / 暗 #333333）
        assert_ne!(
            light.colors.popover_bg, dark.colors.popover_bg,
            "弹层底色随明暗"
        );
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
        assert_ne!(
            light.mermaid_border, dark.mermaid_border,
            "边线色随明暗分流"
        );
    }

    #[gpui::test]
    fn editor_row_bg_tokens_pinned_both_themes(_cx: &mut TestAppContext) {
        // 编辑器行底色三 token 逐字节钉死：选区为**半透明补偿色**（叠编辑区底合成目标
        // 取色值：暗 rgba(0x3ab5e973)≈#245C74 / 亮 rgba(0x619bd373)≈#B8D2EB，保证括号
        // 竖线穿选区透出），折叠/光标行保持不透明（暗 #172931/#242525、亮 #E6EFF8×2）。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(
            dark.editor_selection_bg,
            rgba(0x3ab5e973),
            "暗选区底（半透明补偿色）"
        );
        assert_eq!(dark.editor_fold_bg, rgb(0x172931), "暗折叠行底色");
        assert_eq!(
            dark.editor_active_line_bg,
            rgb(0x242525),
            "暗光标行底色（已与折叠行区分）"
        );
        assert_eq!(
            light.editor_selection_bg,
            rgba(0x619bd373),
            "亮选区底（半透明补偿色）"
        );
        assert_eq!(light.editor_fold_bg, rgb(0xe6eff8), "亮折叠行底色");
        assert_eq!(
            light.editor_active_line_bg,
            rgb(0xe6eff8),
            "亮光标行底色 = 折叠行同色"
        );
        // 选区半透明（竖线透出契约）；折叠/光标行不透明不动。
        assert!(dark.editor_selection_bg.a < 1.0, "暗选区底应半透明（a<1）");
        assert!(light.editor_selection_bg.a < 1.0, "亮选区底应半透明（a<1）");
        assert_eq!(dark.editor_fold_bg.a, 1.0, "暗折叠底应不透明");
        assert_eq!(dark.editor_active_line_bg.a, 1.0, "暗光标行底应不透明");
        // 语义不变式：亮色光标行底色恒等于折叠行底色；暗色三档两两互不相同（折叠冷灰蓝
        // #172931 / 光标行中性灰 #242525 / 选区半透明蓝系另档）。两主题都「折叠/光标
        // 行比选区更贴背景」。
        assert_ne!(
            dark.editor_selection_bg, dark.editor_fold_bg,
            "暗：选区 ≠ 折叠"
        );
        assert_ne!(
            dark.editor_selection_bg, dark.editor_active_line_bg,
            "暗：选区 ≠ 光标行"
        );
        assert_ne!(
            dark.editor_fold_bg, dark.editor_active_line_bg,
            "暗：折叠 ≠ 光标行（两色已区分）"
        );
        assert_eq!(
            light.editor_active_line_bg, light.editor_fold_bg,
            "亮：光标行=折叠行"
        );
        // 同同档值随明暗分离（暗深 / 亮浅）。
        assert_ne!(
            dark.editor_selection_bg, light.editor_selection_bg,
            "选区底色随明暗"
        );
        assert_ne!(dark.editor_fold_bg, light.editor_fold_bg, "折叠底色随明暗");
        assert_ne!(
            dark.editor_active_line_bg, light.editor_active_line_bg,
            "光标行底色随明暗"
        );
    }

    #[gpui::test]
    fn find_highlight_tokens_pinned_both_themes(_cx: &mut TestAppContext) {
        // 编辑器查找高亮两 token：当前命中（强橙）与非当前（弱黄）亮暗两套都钉死，
        // 且「当前 ≠ 非当前」保证导航所指一目了然。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(light.editor_find_match_bg, rgb(0xffe497), "亮非当前=弱黄");
        assert_eq!(light.editor_find_current_bg, rgb(0xffb703), "亮当前=强橙");
        assert_eq!(
            dark.editor_find_match_bg,
            rgb(0x4a3f16),
            "暗非当前=弱黄暗档"
        );
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
    fn doc_selection_bg_pinned_both_themes(_cx: &mut TestAppContext) {
        // 文档选区底色 token：半透蓝（markdown 预览 / WYSIWYG / PDF 阅读器共用）。
        // 亮暗值 = 收编前各消费方原值逐字节（暗 0x4d6bfe61 / 亮 0x4d6bfe42），零观感回归；
        // 半透明（a<1）保证叠在纸面/正文上透出下方内容。
        let light = Theme::light().colors;
        let dark = Theme::dark().colors;
        assert_eq!(
            light.doc_selection_bg,
            rgba(0x4d6bfe42),
            "亮文档选区底=原 rgba(0x4d6bfe42)"
        );
        assert_eq!(
            dark.doc_selection_bg,
            rgba(0x4d6bfe61),
            "暗文档选区底=原 rgba(0x4d6bfe61)"
        );
        assert_ne!(
            light.doc_selection_bg, dark.doc_selection_bg,
            "文档选区底色随明暗"
        );
        assert!(
            light.doc_selection_bg.a < 1.0,
            "亮文档选区底应半透明（a<1）"
        );
        assert!(dark.doc_selection_bg.a < 1.0, "暗文档选区底应半透明（a<1）");
        // 与按钮选中基底 / 编辑器选区补偿色彼此可辨（不同色相/透明度档，不可混用）。
        assert_ne!(
            light.doc_selection_bg, light.selected_bg,
            "亮文档选区 ≠ 按钮选中基底"
        );
        assert_ne!(
            dark.doc_selection_bg, dark.selected_bg,
            "暗文档选区 ≠ 按钮选中基底"
        );
        assert_ne!(
            light.doc_selection_bg, light.editor_selection_bg,
            "亮文档选区 ≠ 编辑器选区"
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
        assert_eq!(
            dark.bar_border,
            rgba(0xffffff1a),
            "暗工具条分隔边框=内容 border"
        );
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
        assert!(
            lc.selected_bg.a < lc.pressed_bg.a,
            "亮按压 alpha 应大于选中"
        );
        assert!(
            dc.selected_bg.a < dc.pressed_bg.a,
            "暗按压 alpha 应大于选中"
        );
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
        assert_eq!(
            light.hairline,
            rgb(0xd9d9d9),
            "亮发丝线=原 SEPARATOR_LIGHT #D9D9D9"
        );
        assert_eq!(
            dark.hairline,
            rgb(0x2a2b2c),
            "暗发丝线=原 SEPARATOR_DARK #2A2B2C"
        );
        assert_ne!(light.hairline, dark.hairline, "发丝线随明暗切换");
        assert_eq!(
            light.scrollbar_thumb,
            rgba(0x6060604a),
            "滚动条滑块=原 shell rgba(0x6060604a)"
        );
        assert_eq!(dark.scrollbar_thumb, rgba(0x6060604a), "滚动条滑块亮暗同值");
        assert_eq!(
            light.scrollbar_thumb, dark.scrollbar_thumb,
            "滚动条滑块亮暗持同值（亮暗同值字段）"
        );
    }

    #[gpui::test]
    fn card_shadow_opaque_black_locks_shadow_base_contract(_cx: &mut TestAppContext) {
        // 判别式（④-2 浮层投影收编）：menu/notification/tooltip/find/goto 卡片与 video 控制条
        // 的投影底色一律取 colors.card_shadow。默认零覆盖下它必须 = 不透明黑
        // （rgb(0x000000)、α=1）→ `card_shadow.opacity(α)` 与旧散值 `hsla(0,0,0,α)` 逐通道同值；
        // 撤掉该前提（改成非黑/非不透明）→ 此处即红，投影将不再与旧观感逐字节不变。
        for scheme in [ColorScheme::light(), ColorScheme::dark()] {
            assert_eq!(scheme.card_shadow, rgb(0x000000), "卡片投影底=不透明黑");
            assert_eq!(scheme.card_shadow.a, 1.0, "卡片投影底 α=1（不透明）");
        }
    }

    #[gpui::test]
    fn chrome_state_colors_are_pinned(_cx: &mut TestAppContext) {
        // P1-7：外壳层状态色钉死——关闭按压红亮暗同值；拖拽幽灵随明暗且 = 强调色。
        let light = ChromeColors::light();
        let dark = ChromeColors::dark();
        assert_eq!(light.close_pressed, rgb(0xa70d1a), "亮关闭按压红");
        assert_eq!(
            dark.close_pressed,
            rgb(0xa70d1a),
            "暗关闭按压红 = 亮档（危险红同值）"
        );
        assert_eq!(
            light.close_pressed, dark.close_pressed,
            "关闭按压红亮暗同值"
        );
        assert_ne!(
            light.close_pressed, light.close_hover,
            "按压比 hover 深一档"
        );
        assert_eq!(
            light.drag_ghost_bg,
            ColorScheme::light().accent,
            "亮拖拽幽灵 = 亮强调色"
        );
        assert_eq!(
            dark.drag_ghost_bg,
            ColorScheme::dark().accent,
            "暗拖拽幽灵 = 暗强调色"
        );
        assert_ne!(light.drag_ghost_bg, dark.drag_ghost_bg, "拖拽幽灵随明暗");
        // 选项卡拖拽排序插入指示线：纯白、亮暗同值（原 shell_tabs 裸 gpui::white()）。
        assert_eq!(light.tab_reorder_line, rgb(0xffffff), "亮插入指示线=纯白");
        assert_eq!(dark.tab_reorder_line, rgb(0xffffff), "暗插入指示线=纯白");
        assert_eq!(
            light.tab_reorder_line, dark.tab_reorder_line,
            "插入指示线亮暗同值（白色不随外壳底）"
        );
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
        assert_eq!(
            dark.editor_bg,
            rgb(0x121314),
            "暗编辑区底=原 EDITOR_BG_DARK"
        );
        assert_eq!(dark.chrome_bg, rgb(0x191a1b), "暗外壳底=原 CHROME_BG_DARK");
        assert_ne!(light.editor_bg, dark.editor_bg, "编辑区底随明暗");
        assert_ne!(light.chrome_bg, dark.chrome_bg, "外壳底随明暗");
        // 与保留的 panel_bg 分道：暗档 editor_bg(#121314) ≠ panel_bg(#2B2B2B)，
        // panel_bg 及其不变式逐字节保留（不互相替代）。
        assert_eq!(
            dark.panel_bg,
            rgb(0x2b2b2b),
            "暗 panel_bg 原样保留（随 colors.bg）"
        );
        assert_ne!(
            dark.editor_bg, dark.panel_bg,
            "暗编辑区底 ≠ 暗 panel_bg（语义分离）"
        );
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
            assert_eq!(
                colors.surface, colors.popover_bg,
                "浮动层=升一级表面（surface 同 popover 底）"
            );
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

    // ── 全站语义色主题系统 · ui 模型层测试（2026-11-22）──

    /// 从 `(key, value)` 对构建 custom 覆写表。
    fn custom(map: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, String> {
        map.iter().map(|&(k, v)| (k, v.into())).collect()
    }

    /// `music` 预设指针（供多处测试复用）。
    fn music_preset() -> &'static PresetSkin {
        PRESETS.iter().find(|p| p.id == "music").expect("music 预设存在")
    }

    #[gpui::test]
    fn theme_token_key_roundtrips_and_unknown_returns_none(_cx: &mut TestAppContext) {
        // 全部字段的 key()/from_key() 双向往返一致；未知 key 返回 None（调用方兜底不 panic）。
        const ALL: &[ColorToken] = &[
            ColorToken::Bg, ColorToken::Surface, ColorToken::Fg, ColorToken::FgMuted,
            ColorToken::Border, ColorToken::Accent, ColorToken::Link, ColorToken::Error,
            ColorToken::FgDisabled, ColorToken::FocusRing, ColorToken::SelectedBg,
            ColorToken::PressedBg, ColorToken::DragHoverBorder, ColorToken::CardShadow,
            ColorToken::CodeBlockBg, ColorToken::CodeInlineFg, ColorToken::CodeInlineBg,
            ColorToken::EditorCodeBg, ColorToken::QuoteBg, ColorToken::TableHeadBg,
            ColorToken::Heading, ColorToken::PopoverBg, ColorToken::MermaidAccentPalette,
            ColorToken::MermaidBorder, ColorToken::EditorSelectionBg, ColorToken::EditorFoldBg,
            ColorToken::EditorActiveLineBg, ColorToken::EditorFindMatchBg,
            ColorToken::EditorFindCurrentBg, ColorToken::GuideInactive, ColorToken::GuideActive,
            ColorToken::BracketGuidePalette, ColorToken::DocSelectionBg,
            ColorToken::SrfDeep, ColorToken::SrfPrimary, ColorToken::SrfSecondary,
            ColorToken::SrfCard, ColorToken::SrfHover, ColorToken::SrfBorder,
            ColorToken::AccentLight, ColorToken::OverlayWhite,
            ColorToken::Success, ColorToken::Warning, ColorToken::Info, ColorToken::Favorite,
            ColorToken::WindowBg, ColorToken::MaterialTop, ColorToken::MaterialBottom,
            ColorToken::BarBg, ColorToken::BarBorder, ColorToken::Hairline, ColorToken::PanelBg,
            ColorToken::EditorBg, ColorToken::ChromeBg, ColorToken::TextPrimary,
            ColorToken::TextMinor, ColorToken::TextTertiary, ColorToken::ChipBg,
            ColorToken::ChipText, ColorToken::TabActiveBg, ColorToken::TabActiveBorder,
            ColorToken::PillBg, ColorToken::PillFg, ColorToken::Dirty, ColorToken::HoverFill,
            ColorToken::PressedFill, ColorToken::CloseHover, ColorToken::ClosePressed,
            ColorToken::DragGhostBg, ColorToken::MenuHighlight, ColorToken::ScrollbarThumb,
            ColorToken::TabReorderLine,
        ];
        for &t in ALL {
            assert_eq!(
                ColorToken::from_key(t.key()),
                Some(t),
                "key 往返一致：{}",
                t.key()
            );
            // 文本寻址风格锚：colors.* / chrome.* 两域名前缀。
            assert!(
                t.key().starts_with("colors.") || t.key().starts_with("chrome."),
                "键前缀必须 colors./chrome.：{}",
                t.key()
            );
        }
        // UI 可暴露子集是合法令牌（key 均可往返）。
        for &t in UI_EXPOSABLE_TOKENS {
            assert_eq!(
                ColorToken::from_key(t.key()),
                Some(t),
                "UI 暴露子集 key 合法：{}",
                t.key()
            );
        }
        assert_eq!(ColorToken::from_key("colors.nope"), None, "未知 key 返回 None");
        assert_eq!(ColorToken::from_key("chrome.nope"), None);
    }

    #[gpui::test]
    fn parse_hex_accepts_six_and_eight_digit(_cx: &mut TestAppContext) {
        // #rrggbb 6 位（不透明）与 #rrggbbaa 8 位（保留 alpha，P2-2）：
        // 合法 → Rgba；缺 # / 长度非 6/8 / 非法 hex → None。
        assert_eq!(ColorToken::parse_hex("#ff0000"), Some(rgb(0xff0000)));
        assert_eq!(ColorToken::parse_hex("#0d0b1a"), Some(rgb(0x0d0b1a)));
        assert_eq!(ColorToken::parse_hex("#FFFFFF"), Some(rgb(0xffffff)), "大写 hex 亦可");
        // 8 位：r/g/b + alpha 逐字节保留。
        assert_eq!(
            ColorToken::parse_hex("#ffffff0f"),
            Some(rgba(0xffffff0f)),
            "white 6% 半透明边框（预设 srf_border 字面）"
        );
        assert_eq!(
            ColorToken::parse_hex("#11223380"),
            Some(rgba(0x11223380)),
            "8 位 α=0x80"
        );
        assert_eq!(
            ColorToken::parse_hex("#00000000"),
            Some(rgba(0x00000000)),
            "8 位 α=0x00"
        );
        let a8 = ColorToken::parse_hex("#ff000080").expect("合法 8 位");
        assert!(
            (a8.a - ((0x80 as f32) / 255.0)).abs() < 1e-6,
            "α 值保留（0x80/255）"
        );
        // 非法：缺 # / 长度非 6/8 / 非 hex。
        assert_eq!(ColorToken::parse_hex("ff0000"), None, "缺 # → None");
        assert_eq!(ColorToken::parse_hex("#ff000"), None, "5 位 → None");
        assert_eq!(ColorToken::parse_hex("#ff00000"), None, "7 位 → None");
        assert_eq!(ColorToken::parse_hex("#ff0000000"), None, "9 位 → None");
        assert_eq!(ColorToken::parse_hex("#gg0000"), None, "非法 hex 字符 → None");
        assert_eq!(ColorToken::parse_hex("#zzzzzzzz"), None, "8 位非法 hex → None");
        assert_eq!(ColorToken::parse_hex("#"), None);
        assert_eq!(ColorToken::parse_hex(""), None);
    }

    #[gpui::test]
    fn merge_default_preset_is_byte_identical(_cx: &mut TestAppContext) {
        // 零回归锚：default 预设（空覆盖）+ 空 custom → 合并结果与 `ColorScheme::light()/
        // dark()`/`ChromeColors::light()/dark()` **逐字段字节相等**（整结构断言）。
        // 判别式：撤掉合并改回直用 base 即 pass；合并误改任一既有字段即 fail。
        let default = &PRESETS[0];
        let empty = BTreeMap::new();
        assert_eq!(
            merge_spectrum(ThemeMode::Light, &ColorScheme::light(), default, &empty),
            ColorScheme::light(),
            "亮档 default 合并必须逐字段等于 base"
        );
        assert_eq!(
            merge_spectrum(ThemeMode::Dark, &ColorScheme::dark(), default, &empty),
            ColorScheme::dark(),
            "暗档 default 合并必须逐字段等于 base"
        );
        assert_eq!(
            merge_chrome(&ChromeColors::light(), &empty),
            ChromeColors::light(),
            "亮 chrome 空合并不变"
        );
        assert_eq!(
            merge_chrome(&ChromeColors::dark(), &empty),
            ChromeColors::dark(),
            "暗 chrome 空合并不变"
        );
        // 新字段缺省值锚（零覆盖时的基座值，供后续对比阶段对照）。
        let dark = ColorScheme::dark();
        assert_eq!(dark.srf_deep, rgb(0x121314), "暗面坡深=中性");
        assert_eq!(dark.srf_border, rgba(0xffffff0f), "暗表分隔=白6%");
        assert_eq!(dark.overlay_white, rgb(0xffffff), "覆盖白=纯白");
    }

    #[gpui::test]
    fn merge_music_preset_hits_personality_tokens(_cx: &mut TestAppContext) {
        // preset 命中：music 暗档 accent/srf_deep/accent_light 逐字节对齐。
        let merged = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &BTreeMap::new(),
        );
        assert_eq!(merged.accent, rgb(0x7c5cfc), "music 暗档 accent=#7C5CFC");
        assert_eq!(merged.srf_deep, rgb(0x0d0b1a), "music 暗档 srf_deep=#0D0B1A");
        assert_eq!(merged.accent_light, rgb(0xa78bfa), "music 暗档 accent_light=#A78BFA");
        // 未列入人格表的底色保持 base（音乐紫不动主背景）。
        assert_eq!(merged.bg, ColorScheme::dark().bg, "未覆盖字段保持 base");
    }

    #[gpui::test]
    fn merge_custom_overrides_preset(_cx: &mut TestAppContext) {
        // custom > preset：music + custom accent=#FF0000 → accent 被 custom 盖过。
        let merged = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &custom(&[("colors.accent", "#ff0000")]),
        );
        assert_eq!(merged.accent, rgb(0xff0000), "custom 覆盖 preset accent");
        assert_eq!(merged.srf_deep, rgb(0x0d0b1a), "preset 其余令牌不受影响");
        assert_eq!(merged.accent_light, rgb(0xa78bfa), "未经 custom 的令牌保持 preset");
    }

    #[gpui::test]
    fn merge_custom_default_reverts_to_preset(_cx: &mut TestAppContext) {
        // 单 key "default"：还原到 preset 值（music accent 复原为 #7C5CFC，而非 base）。
        let merged = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &custom(&[("colors.accent", "default")]),
        );
        assert_eq!(merged.accent, rgb(0x7c5cfc), "「default」还原到 preset accent");
        assert_eq!(merged.srf_deep, rgb(0x0d0b1a), "preset 其余仍生效");
    }

    #[gpui::test]
    fn merge_skips_invalid_hex_and_unknown_keys(_cx: &mut TestAppContext) {
        // 非法 hex / 未知 key → 跳过该条、不影响其它 token、不 panic。
        let merged = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &custom(&[
                ("colors.accent", "nothex"),       // 非 hex
                ("colors.link", "#zzzzzz"),        // 非法 hex 字符
                ("colors.unknown_key", "#ff0000"), // 未知 key
                ("colors.srf_deep", "#000000"),    // 合法 → 应生效
            ]),
        );
        assert_eq!(
            merged.accent,
            rgb(0x7c5cfc),
            "非法 hex 的 key 被跳过 → accent 保持 preset"
        );
        assert_eq!(
            merged.link,
            ColorScheme::dark().link,
            "非法 hex 的 key 被跳过 → link 保持 base"
        );
        assert_eq!(merged.srf_deep, rgb(0x000000), "合法 key 正常生效");
    }

    #[gpui::test]
    fn merge_modes_are_independent(_cx: &mut TestAppContext) {
        // 两档独立：亮/暗合并互不污染。
        let music = music_preset();
        let light = merge_spectrum(ThemeMode::Light, &ColorScheme::light(), music, &BTreeMap::new());
        let dark = merge_spectrum(ThemeMode::Dark, &ColorScheme::dark(), music, &BTreeMap::new());
        assert_eq!(dark.accent, rgb(0x7c5cfc), "暗档用 music 暗表");
        assert_eq!(dark.srf_deep, rgb(0x0d0b1a), "暗档深紫坡");
        assert_eq!(light.accent, rgb(0x5b3fd1), "亮档用 music 亮表（协调深紫）");
        assert_ne!(dark.srf_deep, light.srf_deep, "暗坡≠亮坡");
        assert_ne!(light.srf_deep, ColorScheme::light().srf_deep, "亮档 preset 生效");
    }

    #[gpui::test]
    fn set_is_default_preset_thin_wrapper(cx: &mut TestAppContext) {
        // Theme::set = apply(default 预设 + 空覆写)：既有调用方语义零改动。
        cx.update(|cx| Theme::set(ThemeMode::Dark, cx));
        cx.update(|cx| {
            let t = Theme::global(cx);
            assert!(t.is_dark());
            assert_eq!(t.colors, ColorScheme::dark(), "set → 逐字段等于 dark()");
            assert_eq!(t.chrome, ChromeColors::dark(), "chrome 同源 dark()");
        });
        cx.update(|cx| Theme::set(ThemeMode::Light, cx));
        cx.update(|cx| {
            assert_eq!(
                Theme::global(cx).colors,
                ColorScheme::light(),
                "set 亮档逐字段等于 light()"
            );
        });
    }

    #[gpui::test]
    fn apply_writes_merged_theme_and_chrome(cx: &mut TestAppContext) {
        // 端到端：apply(music + custom chrome) → 全局主题同时反映 preset 合并与 chrome 覆写。
        cx.update(|cx| {
            let mut c = BTreeMap::new();
            c.insert("chrome.window_bg", "#112233".into());
            Theme::apply(
                ThemeSpec {
                    mode: ThemeMode::Dark,
                    preset: music_preset(),
                    custom: c,
                },
                cx,
            );
            let t = Theme::global(cx);
            assert!(t.is_dark());
            assert_eq!(t.colors.accent, rgb(0x7c5cfc), "preset 合并生效");
            assert_eq!(t.colors.srf_deep, rgb(0x0d0b1a), "preset srf 生效");
            assert_eq!(t.chrome.window_bg, rgb(0x112233), "custom chrome 覆写生效");
            assert_eq!(t.chrome.bar_bg, ChromeColors::dark().bar_bg, "未覆写 chrome 保持默认");
        });
    }

    // ── P1-1：chrome 从「合并后 colors」重建（杜绝派生漂移）──

    #[gpui::test]
    fn from_scheme_matches_for_mode_on_default_scheme(_cx: &mut TestAppContext) {
        // 零回归锚：`from_scheme` 对默认 scheme 必须与旧 `ChromeColors::for_mode` **逐字段
        // 字节相等**（撤掉 from_scheme 重映射 / 任一 scheme 派生字段抄错即红）。
        assert_eq!(
            ChromeColors::from_scheme(&ColorScheme::light(), ThemeMode::Light),
            ChromeColors::light(),
            "亮档 from_scheme(默认) == for_mode(false)"
        );
        assert_eq!(
            ChromeColors::from_scheme(&ColorScheme::dark(), ThemeMode::Dark),
            ChromeColors::dark(),
            "暗档 from_scheme(默认) == for_mode(true)"
        );
    }

    #[gpui::test]
    fn apply_custom_colors_rebuilds_chrome_from_merged(cx: &mut TestAppContext) {
        // P1-1 判别式：custom 改 `colors.*`（如 bg），chrome 派生字段必须跟 merged colors
        // 走 —— `panel_bg`/`tab_active_bg` 等「同名字段」不再停留在 base 值。
        cx.update(|cx| {
            Theme::apply(
                ThemeSpec {
                    mode: ThemeMode::Light,
                    preset: &PRESETS[0],
                    custom: custom(&[
                        ("colors.bg", "#112233"),
                        ("colors.fg", "#334455"),
                        ("colors.accent", "#556677"),
                    ]),
                },
                cx,
            );
            let t = Theme::global(cx);
            assert_eq!(t.colors.bg, rgb(0x112233), "merged colors.bg");
            // 同名字段物理同值不变式（theme.rs 364-367）：chrome 派生字段 = merged colors 同名字段。
            assert_eq!(t.chrome.panel_bg, t.colors.bg, "panel_bg 重建自合并后 colors.bg");
            assert_eq!(t.chrome.tab_active_bg, t.colors.bg, "tab_active_bg 同源 bg");
            assert_eq!(t.chrome.text_primary, t.colors.fg, "text_primary 同源 fg");
            assert_eq!(t.chrome.text_minor, t.colors.fg_muted, "text_minor 同源 fg_muted");
            assert_eq!(t.chrome.chip_text, t.colors.fg, "chip_text 同源 fg");
            assert_eq!(t.chrome.pill_fg, t.colors.fg, "pill_fg 同源 fg");
            assert_eq!(t.chrome.bar_border, t.colors.border, "bar_border 同源 border");
            assert_eq!(t.chrome.tab_active_border, t.colors.border);
            assert_eq!(t.chrome.dirty, t.colors.error, "dirty 同源 error");
            assert_eq!(t.chrome.drag_ghost_bg, t.colors.accent, "drag_ghost_bg 同源 accent");
            // 外壳专属常量不受 colors 影响（从 light/dark 档既有值沿袭）。
            assert_eq!(t.chrome.window_bg, ChromeColors::light().window_bg, "外壳专属常量保持");
            assert_eq!(t.chrome.editor_bg, ChromeColors::light().editor_bg);
        });
    }

    #[gpui::test]
    fn apply_default_preset_chrome_equals_for_mode(cx: &mut TestAppContext) {
        // P1-1 零回归锚：default 预设 + 空 custom → `apply` 后的 chrome 仍与旧
        // `ChromeColors::for_mode` 逐字段字节相等（撤 from_scheme 重映射即红）。
        cx.update(|cx| {
            Theme::apply(ThemeSpec::plain(ThemeMode::Dark), cx);
            assert_eq!(Theme::global(cx).chrome, ChromeColors::dark(), "暗档默认 chrome=for_mode(true)");
            Theme::apply(ThemeSpec::plain(ThemeMode::Light), cx);
            assert_eq!(Theme::global(cx).chrome, ChromeColors::light(), "亮档默认 chrome=for_mode(false)");
        });
    }

    // ── 阶段④-1：每预设×明暗档 WCAG 对比度回归（P1-6）+ P2-2 8 位 hex 处置 ──

    /// WCAG 2.x 相对亮度：sRGB 通道分段线性/伽马。
    fn linearize(ch: f64) -> f64 {
        if ch <= 0.03928 {
            ch / 12.92
        } else {
            ((ch + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 相对亮度 `L = 0.2126 R + 0.7152 G + 0.0722 B`（参与对比的令牌均不透明）。
    fn luminance(c: Rgba) -> f64 {
        0.2126 * linearize(c.r as f64)
            + 0.7152 * linearize(c.g as f64)
            + 0.0722 * linearize(c.b as f64)
    }

    /// WCAG 对比度 `(L亮 + 0.05) / (L暗 + 0.05)`（≥1）。
    fn contrast_ratio(a: Rgba, b: Rgba) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// 每预设 × 明暗档的对比度回归（阶段④-1，P1-6）：
    /// - `fg`（text_primary 系）vs `srf_deep`（暗档）/`srf_primary`（亮档）≥ 4.5:1（正文）。
    /// - `accent` vs 同档 `srf_deep` 与 `srf_card` ≥ 3:1（UI 组件强调色）。
    /// - `accent_light`（限列于该档预设表内）vs `srf_deep` 与 `srf_card` ≥ 3:1。
    ///
    /// 全表数据（preset / mode / fg对表面 / acc对deep / acc对card / al对deep / al对card；
    /// 阈值 fg≥4.5、acc≥3、al≥3；仅预设表内 AccentLight 才计 al 行；default 无 al 行）：
    ///   default L 14.02 5.78 4.90 (—) ；default D 14.90 10.34 8.58 (—)
    ///   music   L 14.02 6.33 5.00  3.90  3.08 ；music   D 15.58 4.44 3.66  7.15  5.89
    ///   ocean   L 14.60 3.68 3.09  3.68  3.09 ；ocean   D 15.14 10.15 8.23 12.78 10.36
    ///   solar   L 14.55 4.75 3.99  3.71  3.11 ；solar   D 14.66 8.52 6.85 10.96  8.81
    ///   rose    L 14.33 5.67 4.64  4.32  3.53 ；rose    D 14.90 7.02 5.86 10.25  8.56
    ///   graphite L 14.03 6.00 4.77 4.17  3.31 ；graphite D 15.25 11.80 9.87 15.63 13.07
    ///
    /// 亮档修正记录（2026-11-22，对比度修正 #1-#5；改色保持人格、解阈值禁放宽）：
    ///   #1 music   accent_light #a78bfa→#8468dc（原 vs 卡片坡 1.99 <3）
    ///   #2 ocean   accent       #0d9488→#0c9185（原 vs 卡片坡 2.98 <3）
    ///   #3 ocean   accent_light #14b8a6→#109185（原 vs 亮面坡 1.98 <3）
    ///   #4 solar   accent_light #d97706→#c26a05（原 vs 卡片坡 2.53 <3）
    ///   #5 graphite accent_light #7a8087→#717880（原 vs 卡片坡 2.95 <3）
    /// 判别式：任一条阈值被未来改色破坏 → 对应 assert 红（合法 hex 数据缺陷，非放宽阈值）。
    #[gpui::test]
    fn preset_contrast_meets_wcag_per_mode(_cx: &mut TestAppContext) {
        for preset in PRESETS {
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                let empty = BTreeMap::new();
                let colors = merge_spectrum(
                    mode,
                    &ColorScheme::for_mode(matches!(mode, ThemeMode::Dark)),
                    preset,
                    &empty,
                );
                // 正文表面：亮档主表面 srf_primary / 暗档最深表面 srf_deep。
                let text_bg = if matches!(mode, ThemeMode::Dark) {
                    colors.srf_deep
                } else {
                    colors.srf_primary
                };
                let fg_r = contrast_ratio(colors.fg, text_bg);
                assert!(
                    fg_r >= 4.5,
                    "{} {:?} fg vs 表面 = {:.2}:1 < 4.5（正文）",
                    preset.id,
                    mode,
                    fg_r
                );
                // 强调色 vs 同档 srf_deep 与 srf_card（最深/最浅两表面皆须达标）。
                let (acc_deep, acc_card) = (
                    contrast_ratio(colors.accent, colors.srf_deep),
                    contrast_ratio(colors.accent, colors.srf_card),
                );
                assert!(
                    acc_deep >= 3.0 && acc_card >= 3.0,
                    "{} {:?} accent vs srf_deep={:.2} / srf_card={:.2} < 3:1",
                    preset.id,
                    mode,
                    acc_deep,
                    acc_card
                );
                // accent_light：仅当该档预设表显式列出（"若在表内"）才检查。
                let table = if matches!(mode, ThemeMode::Dark) { preset.dark } else { preset.light };
                if table.iter().any(|(t, _)| *t == ColorToken::AccentLight) {
                    let (al_deep, al_card) = (
                        contrast_ratio(colors.accent_light, colors.srf_deep),
                        contrast_ratio(colors.accent_light, colors.srf_card),
                    );
                    assert!(
                        al_deep >= 3.0 && al_card >= 3.0,
                        "{} {:?} accent_light vs srf_deep={:.2} / srf_card={:.2} < 3:1",
                        preset.id,
                        mode,
                        al_deep,
                        al_card
                    );
                }
            }
        }
    }

    #[gpui::test]
    fn merge_applies_8bit_srf_border_with_alpha(_cx: &mut TestAppContext) {
        // P2-2 决策 (a)：`parse_hex` 支持 `#rrggbbaa` 后，含 alpha 的 `srf_border`
        // 人格令牌诚实落位（此前 8 位被跳过 → border 恒 base）。
        // 判别式：撤掉 8 位支持 → ② 的 custom 8 位覆写被跳过 → 保持 base，此断言红。
        // ① 预设表 8 位字面 `#ffffff0f` 现可解析（逐字节 = base 值，数据本身无观感回归）。
        let merged = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &BTreeMap::new(),
        );
        assert_eq!(merged.srf_border, rgba(0xffffff0f), "music 暗档 8 位 srf_border 生效");
        // ② custom 8 位覆写全链生效：r/g/b 与 α 都被保留（真正的判别支）。
        let merged2 = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &custom(&[("colors.srf_border", "#11223380")]),
        );
        assert_eq!(merged2.srf_border, rgba(0x11223380), "custom 8 位 srf_border 生效");
        assert!(
            (merged2.srf_border.a - ((0x80 as f32) / 255.0)).abs() < 1e-6,
            "α=0x80 保留"
        );
        // ③ 6 位 srf_border 仍不透明（不引入 over-engineering 的 alpha 分支）。
        let merged3 = merge_spectrum(
            ThemeMode::Dark,
            &ColorScheme::dark(),
            music_preset(),
            &custom(&[("colors.srf_border", "#123456")]),
        );
        assert_eq!(merged3.srf_border, rgb(0x123456), "6 位 srf_border 不透明");
        assert_eq!(merged3.srf_border.a, 1.0, "6 位 α=1.0");
    }
}
