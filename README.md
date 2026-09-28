# gpui-editor

基于 **Rust + GPUI 0.2.2** 原生实现的文本 / 代码 / Markdown 编辑器（多文件选项卡）。
界面为「菜单栏 + 选项卡栏 + 编辑区 + 侧栏 + 可选 Markdown 实时预览区 + 状态栏」布局。
**所有 UI 组件均为自研**（`src/ui/`），不再依赖 `gpui-component`。

## 功能特性

- **多文件选项卡**（`app/tabs.rs`）：「文件 → 新建 / `Ctrl+N` / 工具栏 +」新建选项卡，内容彼此独立；点击切换，点 × 关闭；**关闭未保存的选项卡时弹出原生确认框**（保存 / 不保存 / 取消），最后一个选项卡关闭退化为空白页。当**当前选中的文件是 Markdown（`.md` / `.markdown`）**时，选项卡栏最右侧显示一个**眼睛图标**：蓝色=已开启预览、灰色=已关闭，点击即切换当前文件的 Markdown 实时预览（与「查看 → 预览」/ `Ctrl+P` 效果一致）。
- **系统文件对话框**（`app/file_ops.rs`，`rfd` 原生对话框）：
  - **打开**：`Ctrl+O` / 菜单「文件 → 打开」弹出系统「打开文件」对话框；
  - **保存**：`Ctrl+S` 直接保存到当前文件；未命名时弹出「保存文件」对话框（即另存为）；
  - **另存为**：菜单「文件 → 另存为」弹出系统保存对话框。
- **拖放打开**：从资源管理器把文件拖到**编辑区 / 预览区 / 窗口空白处**直接打开。
- **菜单栏**（`app/menu.rs`，下拉面板由 `ui/menu.rs` 自绘）：文件（新建 / 打开 / 保存 / 另存为 / 退出）、编辑（撤销 / 重做 / 剪切 / 复制 / 粘贴 / 全选）、查看（预览开关 / 字号调节）、帮助；下拉面板带悬停高亮，点击空白关闭。
- **自研代码编辑器**（`src/editor/`）：`CodeEditor` 基于 `ropey::Rope` 与本地 `RopeExt`（行 / 偏移 / UTF-16 换算），提供语法高亮（`src/syntax/` 自研 `GitHubTheme` + `SemanticStyleResolver`）、行号、缩进参考线、折叠、选择、撤销 / 重做与滚动；IME 组合输入与 UTF-16 光标落点支持；粘贴时统一 Windows CRLF 为 LF。
- **Markdown 虚拟化预览**（自研 `src/markdown/`）：把源码按行扫描成渲染行（标题 / 代码围栏 / 引述 / 列表 / 表格 / 图片 / 分隔线 / 行内样式），用 **GPUI `List` 做行级虚拟化**——只构建 / 绘制可见行，滚动与文档大小解耦；滚动条叠加在内容之上，不会被不透明背景盖住。
- **Markdown 实时编辑预览（WYSIWYG）**（`src/markdown/wysiwyg/`）：所见即所得编辑，打字时以「廉价帧」策略（有界重锚 + 滚动延后上色）保障滚动条跟手；支持 **Mermaid 矢量渲染**（流程图 / 时序图）与 **LaTeX / KaTeX 数学排版**（`$$…$$` / `$…$` / `\begin{...}`）。
- **PDF 阅读器**（`src/pdf/`）：基于 `pdfium-render` 的后台 worker 栅格化，页码缓存 + 视口瓦片虚拟化 + 文本层对齐 + 全文档搜索（搜索期间冻结渲染探针）。
- **EPUB 阅读器**（`src/epub/`）：HTML 解析（`roxmltree`）+ 排版 + 文本渲染，脚注标记 `[!N]` 兼容。
- **目录 / 大纲**（`src/toc/`）：Markdown 标题层级与折叠可见性。
- **编辑可用性增强**：
  - **撤销 / 重做**：`src/editor/undo.rs` 维护编辑历史；`Ctrl+Z` / `Ctrl+Y` 与「编辑」菜单均可用；
  - **状态栏**（`app/status_bar.rs`）：文件名 + 未保存「● 脏标记」+ 语言徽标，实时 **Ln / Col**、行数、字数，以及字号 −/+ 调节（`Ctrl+=` / `Ctrl+-` / `Ctrl+0`）；
  - 每个选项卡独立展示脏标记（●），编辑后立即置脏、保存 / 打开后清除。
- **自绘滚动条**（`ui/scrollbar.rs` 通用组件 + `src/scrollbar.rs` 选项卡栏横向滚动条）：轨道 / 滑块胶囊形、悬停与拖拽放大、指数平滑插值，叠加在内容之上。

## 自研 UI 组件库（`src/ui/`）

GPUI 应用内**全部 UI 组件均为自研**，替代已移除的 `gpui-component`：

| 组件 | 文件 | 说明 |
| --- | --- | --- |
| 主题 / 语义色 | `ui/theme.rs` | 亮 / 暗色板 + Typography + Markdown 语义色（代码块 / 引用 / 表格头 / 标题 / 通知等） |
| 图标 | `ui/icon.rs` | 矢量图标 |
| 按钮 | `ui/button.rs` | 变体 / 标签 / 状态，几何与交互双测试 |
| 输入框 | `ui/input.rs` | 单行输入（IME / 直输、UTF-16 换算、占位符 / 光标 / 选中） |
| 菜单 | `ui/menu.rs` | 下拉菜单（键盘 + 点击两项测） |
| 通知 | `ui/notification.rs` | 自托管通知容器 + 超时自动消失（5s） |
| 窗口根 | `ui/root.rs` | 窗口根视图（主题背景 + rem + Tab / Shift+Tab 焦点循环） |
| 滚动条 | `ui/scrollbar.rs` | 垂直 / 水平自绘滚动条（`ScrollbarHandle` 契约） |

纪律：新 UI 组件进 `src/ui/`，必须带 `gpui::test`（几何 / 交互两路径）。

## 技术栈

| 类别 | 选型 |
| --- | --- |
| 语言 / UI | Rust · GPUI `0.2.2`（无 webview，原生窗口） |
| 文件对话框 | `rfd`（系统原生打开 / 保存对话框） |
| 文本 / 编辑 | `ropey`（缓冲区）· `regex` · `memchr` · `unicode-segmentation` · `unicode-bidi` · `rustc-hash` |
| 语法高亮 | `syntect`（解析）→ 自研 `GitHubTheme` / `SemanticStyleResolver` |
| 数学排版 | `ratex-parser` / `ratex-layout` / `ratex-render`（KaTeX 字体，纯 CPU 光栅） |
| 异步 | `futures`（大文档「分块读盘·渐进装配」通道） |
| PDF | `pdfium-render` |
| EPUB / 压缩 | `roxmltree` · `zip` |
| 图像 | `image`（+ webp） |
| 序列化 | `serde` · `serde_json` |
| 测试 | GPUI `test-support`（`cargo test`，456 通过 / 0 失败） |

## 目录结构（主要代码作用）

```
src/
├── main.rs              # 入口：注册快捷键、打开主窗口
├── actions.rs           # 编辑器 / 预览的 Action 与 keymap 定义
├── app/                 # 主视图外壳：tabs / menu / sidebar / status_bar / file_ops / panes / open_progress
├── config/              # 配置：layout / limits / markdown / math / pdf / timing 常量
├── editor/              # 自研 CodeEditor：buffer / highlight / fold / selection / undo / movement / rope_ext / view
├── markdown/
│   ├── parse/           # Markdown 行扫描解析（blocks / rows / cells / inline_style / citations …）
│   ├── view/            # 虚拟化预览（ListState 行窗口 + row / table / style）
│   └── wysiwyg/         # 实时编辑预览（element / frame / shape / mermaid / math / input / math_typeset）
├── syntax/              # 语法高亮：GitHubTheme + SemanticStyleResolver + 预暖和缓存
├── pdf/                 # PDF 阅读（后台 worker 栅格化 + 瓦片虚拟化 + 搜索）
├── epub/                # EPUB 阅读（HTML 解析 + 排版）
├── toc/                 # 目录 / 大纲
├── ui/                  # 自研 UI 组件库（theme / icon / button / input / menu / notification / root / scrollbar）
├── scrollbar.rs         # 选项卡栏横向自绘滚动条
├── prof.rs              # 性能埋点
└── markdown/mod.rs      # Markdown 渲染行模型（doc / ir）
```

### 核心机制说明

- **代码编辑器**：`src/editor/` 基于 `ropey::Rope` 维护文本缓冲区，`rope_ext::RopeExt` 提供行 / 偏移 / UTF-16 换算；高亮由 `src/syntax/` 自研主题解析器负责（替代已移除的 `gpui-component` 高亮器），代码块带带队内门控（解析完成才按带内上色）。
- **Markdown 虚拟化预览**：`markdown/parse/` 把源码按行转成渲染行；`markdown/view/` 用 GPUI `ListState` 做行级窗口化（只持可见窗口的行）与置顶滚动条；每个选项卡持有一个 `MarkdownPreview`，内容变化经去抖后 `set_source` 更新。
- **WYSIWYG 实时编辑**：`markdown/wysiwyg/` 把渲染行整形（shape）进波段缓存（键 = 结构代 ^ 数学代，滚动 / 带重建复用），Pass1 基础布局三段式增量（前缀沿用 / 中段重估 / 尾部复用）+ 二分定位带内区间；滚动条跟手依赖「廉价帧」策略（有界重锚 + 滚动延后上色）与 band 重建语义。
- **流式 / 分块**：大文档「分块读盘 · 渐进装配」经 `futures` 异步通道推流，主线程定时轮询刷新；表格横向位置 / 选区在流式阶段与结束后的历史轮次间平滑迁移。
- **滚动条**：`ui/scrollbar.rs` 自绘，叠加在内容之上，不会被代码块等不透明背景盖住；拖拽写回 `ScrollbarHandle`，悬停 / 拖拽平滑放大。

## 代码参考出处

本项目 Markdown 渲染方案参考了以下两个 **GPUI 开源项目**（已在本地 `gpui_reference_code` 中研读实现，吸收其「行级虚拟化 / 只渲染可见行 / 高度占位保持滚动几何一致」的思路后自研实现）：

| 项目 | 仓库 | 启发 |
| --- | --- | --- |
| **vellum**（桌面 Markdown 编辑器） | https://github.com/wzzc-dev/vellum | 块级 / 行级虚拟化窗口化（`compute_visible_block_range` + 高度占位）、自绘滚动条、增量解析 |
| **zorite**（Markdown 日记 / 大纲） | https://github.com/packetThrower/zorite | 行级 shape 波段虚拟化与高度缓存、解析去抖、解析耗时预扫描与「昂贵输入先渲染纯文本」策略 |

> 说明：本仓库为独立实现（未拷贝上述项目代码），仅借鉴其性能优化思路；如涉及引用他人代码的部分，以各上游仓库的开源许可为准。

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `Ctrl+S` | 保存（未命名时弹保存对话框） |
| `Ctrl+N` | 新建选项卡 |
| `Ctrl+O` | 打开文件（系统对话框） |
| `Ctrl+P` | 预览开关 |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | 增大 / 减小 / 重置字号 |
| `Ctrl+C` / `Ctrl+V` / `Ctrl+X` | 复制 / 粘贴 / 剪切 |
| `Ctrl+Z` / `Ctrl+Y` | 撤销 / 重做 |
| `Ctrl+A` | 全选 |
| `← → ↑ ↓`（可加 `Shift` / `Ctrl` / `Alt`） | 光标移动 / 选择 / 词级与文档级导航 |
| `Backspace` / `Delete`（可加 `Ctrl`） | 删除上一个 / 下一个词 |
| `Ctrl+Q` | 退出 |

## 构建与运行

PDF 预览依赖 pdfium 动态库：前往 https://github.com/bblanchon/pdfium-binaries/releases 下载，解压得到 `pdfium.dll`，重命名并放于 `resources/pdfium/pdfium-154.0.8035.0.dll`。

```bash
# 运行（开发）
cargo run

# 发布（Release profile 已配 opt-level=z / LTO / strip / panic=abort）
cargo build --release

# 全量测试
cargo test

# Lint
cargo clippy --all-targets
```

### 测试 fixture 说明

部分测试（PDF / EPUB / 图片 / 大文档）依赖 fixture 文件，**未提交至仓库**（`target/` 被 gitignore，`CARGO_MANIFEST_DIR/target`），首次跑 `cargo test` 前需手动放置：

| fixture | 说明 |
| --- | --- |
| `target/pdf测试.pdf` | 87 页文本型 PDF（长文档 / 遍历 / 搜索用例） |
| `target/17. 道路机动车辆产品准入审查要求1.0.pdf.pdf` | 更长 PDF（长文遍历用例，`pdf测试.pdf` 的副本） |
| `target/xy.epub` | 《共产党宣言》6 图 EPUB |
| `target/img测试.png` | 实为 580x773 JPEG（magic 探测） |
| `target/大文档测试.md` / `大文档测试*.txt` | 大文档交互 / 基准用例（含 23MB / 116MB / 126MB 档） |

> 若缺 fixture，将有 38 条环境性测试失败（与代码无关）。`cargo test` 的回归护栏覆盖：滚动条拖动、WYSIWYG 实时滚动、Markdown 预览滚动、编辑器远跳 / 延迟滚动、CJK 字符边界点击、撤销 / 重做、全选替换等。

## 已知限制与后续方向

- 内容区拖选是整条文本维度的选择；图表 / 图片等富媒体尚未支持选择。
- 编辑区内**查找 / 替换 / 高亮全部匹配**尚未实现（现有侧栏搜索框是「查结果列表」，不替代编辑区内「跳到下一个匹配」）。
- Markdown 预览自定义行内样式（加粗 / 斜体 / 行内代码已解析为样式段预留，尚未逐字着色）；复杂表格与链接点击等富特性待后续增强。
- 更多规划见 `docs/future-work.md`（死依赖已清、README 已对齐；后续为巨函数分解、fixture 收仓、CI 等）。
