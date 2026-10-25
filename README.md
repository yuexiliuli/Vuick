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
- **视频播放器**（`src/video/`，FFmpeg 9.0 解码）：支持多格式（MP4 / AVI / MOV…）播放与同文件夹切换；音频时钟 A/V 对齐、音量 / 静音、倍速（0.5–2×）、底部控制条（可拖 seek 进度条 / 音量条 / 倍速循环）；Ctrl+滚轮缩放 + 左键拖拽平移。右键「锁定」进入**纯视频模式**——隐藏全部外壳、画面充满窗口，控制条为浮层（悬停条带浮现、离开 1s 自隐、切视频浮现）。
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
| 编码 | `encoding_rs`（GBK/GB18030/UTF-16 等非 UTF-8 解码；保存按各选项卡源编码回写） |
| PDF | `pdfium-render` |
| EPUB / 压缩 | `roxmltree` · `zip` |
| 图像 | `image`（+ webp） |
| 视频 | `ffmpeg-next`（FFmpeg 9.0 解码 / 重采样）· `cpal`（音频输出） |
| 序列化 | `serde` · `serde_json` |
| 测试 | GPUI `test-support`（`cargo test`，584 通过 / 0 失败） |

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
├── video/               # 视频播放（decode/audio/viewer；纯视频锁定模式 + 浮层控制条）
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

**视频功能依赖 FFmpeg 开发环境**（`ffmpeg-sys-next` 链接期需要）：
- `FFMPEG_DIR` → 指向含 `include/` `lib/` `bin/` 的 FFmpeg **win64 shared** 构建（gyan / BtbN，9.x）；
- `LIBCLANG_PATH` → 指向含 `libclang.dll` 的目录（bindgen 必需，装 LLVM 即得）。

一键装环境：`pwsh scripts/setup-ffmpeg.ps1`（下载 / 配置提示、写用户环境变量，新终端生效）。
跳过视频功能可 `cargo build --workspace --no-default-features --exclude media-ffmpeg`（不编译
`src/video/`，也不构建 `media-ffmpeg` cdylib——否则默认 workspace 构建仍需要 FFmpeg 环境）。

PDF 预览依赖 pdfium 动态库：前往 https://github.com/bblanchon/pdfium-binaries/releases 下载，解压得到 `pdfium.dll`，重命名并放于 `resources/pdfium/pdfium-154.0.8035.0.dll`（构建时若探测到会**内嵌进 exe**，分发免带）。

```bash
# 运行（开发）
cargo run

# 发布（Release profile 已配 opt-level=z / LTO / strip / panic=abort）
cargo build --release

# 分发打包（release exe + media_ffmpeg.dll + FFmpeg 运行库 → dist\gpui-editor-win64\）
pwsh scripts/package.ps1

# 全量测试
cargo test

# Lint
cargo clippy --workspace --all-targets
```

### 测试 fixture 说明

PDF / EPUB / 图片 / 大文档测试的 fixture **已提交至仓库**（`fixtures/`，不入 `target/`），fresh clone 后 `cargo test` 直接全绿，无需手动放置：

| fixture | 说明 |
| --- | --- |
| `fixtures/pdf测试.pdf` | 87 页文本型 PDF（长文档 / 遍历 / 搜索用例；长文遍历用例共用此份） |
| `fixtures/epub测试.epub` | 6 图 EPUB |
| `fixtures/img测试.png` | 实为 580x773 JPEG（magic 探测） |
| `fixtures/大文档测试.rs` / `fixtures/大文档测试2.md` | 6.4MB 大文档（编辑器 / 预览交互与 `BIGDOC_BENCH=1` 延迟基准） |

> 超大文档档位（`大文档测试.md` / `大文档测试23MB*` / `大文档测试116MB.txt` / `大文档测试126MB.txt` 等）体积过大不入库，仅供 `#[ignore]` 手动 / 基准用例按需放置至 `fixtures/`。`cargo test` 的回归护栏覆盖：滚动条拖动、WYSIWYG 实时滚动、Markdown 预览滚动、编辑器远跳 / 延迟滚动、CJK 字符边界点击、撤销 / 重做、全选替换等。

## 发布 / 打包分发

视频解码内核已拆成独立 cdylib（`media-ffmpeg` 工作区成员，产物 `media_ffmpeg.dll`）：
主 exe **不静态导入**任何 FFmpeg dll，只在打开视频时经 libloading **运行时**加载它。
所以：
- **exe 缺 `media_ffmpeg.dll` / FFmpeg 运行库也能正常启动**——不开视频照常使用全部其它功能，开视频时在窗口内提示「缺少视频运行库」；
- 要让视频功能可用，分发时必须把 `media_ffmpeg.dll` 与全部 FFmpeg dll 和 exe **放到同一目录**（清单见下方「视频组件所需 DLL」）。一键打包：

```bash
# 构建 release 并导出「exe + media_ffmpeg.dll + 全部 FFmpeg 运行库」到 dist\gpui-editor-win64\
# 脚本自动校验：exe 无 FFmpeg 静态导入、media_ffmpeg.dll 所需的 FFmpeg dll 全部在包内
pwsh scripts/package.ps1
```

### 视频组件所需 DLL（与 exe 同目录）

| 文件 | 来源 | 说明 |
| --- | --- | --- |
| `media_ffmpeg.dll` | 本仓库 `target\release\`（cdylib） | 视频解码内核：主 exe 打开视频时经 libloading **运行时**加载它 |
| `avcodec-63.dll` | `FFMPEG_DIR\bin` | 音视频编解码 |
| `avformat-63.dll` | `FFMPEG_DIR\bin` | 容器格式（MP4 / MKV / AVI / MOV…）解封装 |
| `avutil-61.dll` | `FFMPEG_DIR\bin` | 通用工具库（内存 / 数学 / 字典…） |
| `swscale-10.dll` | `FFMPEG_DIR\bin` | 图像缩放与像素格式转换（→ BGRA 上屏） |
| `swresample-7.dll` | `FFMPEG_DIR\bin` | 音频重采样（→ F32 立体声） |
| `avdevice-63.dll` | `FFMPEG_DIR\bin` | 输入设备接入 |
| `avfilter-12.dll` | `FFMPEG_DIR\bin` | 音视频滤镜（debug 构建需要；release 构建由 LTO 裁掉、缺了也能用） |

缺其中任何 / 全部 → **exe 照常启动**，打开视频时窗口内提示「缺少视频运行库」。
`scripts/package.ps1` 把 `FFMPEG_DIR\bin` 下全部 `*.dll` 一并拷入包内（以上 7 个 FFmpeg dll 全含），
手工分发请以**与编译期同一 FFmpeg 构建**（版本号一致，防 ABI 错位）为准补全。

- **必须用 release**：debug 版还依赖 `vcruntime140d.dll` / `ucrtbased.dll` 等调试 CRT，他人机器也没有；
- 把 `dist\gpui-editor-win64\` 整个目录（或打成 zip）发给对方，**exe 必须与这些 dll 同目录**；
- 其余依赖（`VCRUNTIME140.dll`、`api-ms-win-crt-*`、`d3dcompiler_47.dll`、`icuuc.dll`）均为
  Windows 10/11 系统自带，无需打包；对方是精简版 / 旧版系统缺 `VCRUNTIME140.dll` 时，安装
  「Microsoft Visual C++ 2015–2022 Redistributable (x64)」即可；
- DLL 须与编译期是**同一个** FFmpeg 构建（脚本直接从 `FFMPEG_DIR\bin` 取，版本号一致，不会 ABI 错位）；
- **PDF 无需额外分发**：pdfium dll 构建时已内嵌进 exe（`embed_pdfium`），运行时自动解出。

## 用户偏好（配置文件）

应用的**用户偏好**（主题 / 字号 / 侧栏宽度）会在运行时持久化成一份 JSON 配置文件（代码：`src/config/user.rs`）：

- **位置**（Windows）：`%APPDATA%\gpui-editor\config.json`
  - 可用环境变量 `GPUI_EDITOR_CONFIG` 指定其它路径（绝对值）；无 `APPDATA` 时回退到当前目录 `gpui-editor-config.json`。
  - 它在用户目录、**不在仓库里**，不会随 git 提交。
- **作用**：退出 / 下次重启时保持这些个性化设置：
  | 字段 | 取值 | 说明 |
  | --- | --- | --- |
  | `theme` | `"follow"` / `"light"` / `"dark"` | 跟随系统 / 强制浅色 / 强制深色（`ui/theme.rs`） |
  | `font_size` | 数字 px | 正文字号；缺失 → 默认 `14.0`（`layout::DEFAULT_FONT_SIZE`） |
  | `sidebar_width` | 数字 px / `null` | 侧栏手动宽度；`null` = 随目录内容自适应 |
- **何时写盘**：切换主题、`Ctrl+=` / `Ctrl+-` / `Ctrl+0`（或菜单 / 状态栏）调节字号、侧栏右缘拖拽结束时（`chrome.rs` / `preview.rs` / `sidebar.rs` 的 `persist_prefs`）。
- **排障提示**：觉得界面（含 EPUB / Markdown 正文）文字整体偏小，先看该文件里的 `font_size` 是否被之前 `Ctrl+滚轮` 缩放写小了——应用内按 `Ctrl+0` 即可恢复默认 14 并持久化，或直接改这个 JSON。

示例：`{"theme":"dark","font_size":14.0,"sidebar_width":null}`

## 会话恢复 / 临时文件

关闭程序后再次启动，自动复原上次关闭前的场景：**未命名选项卡（含内容）原样恢复、打开过的文件按路径重新打开、活动选项卡保持一致**（代码：`crates/shell/src/app/session.rs`，作者嘴里叫"会话"临时文件）。

- **开关**：「文件」菜单「☑ 恢复上次会话 / ☐ 恢复上次会话」点击切换，**默认开启**；关闭状态下不再写入会话，并清理上次遗留，下次启动不恢复。
- **保存时机**：状态变化即持久化，**不依赖退出钩子**（本平台关闭窗口的退出路径不回调 `on_app_quit`）——打开 / 新建 / 关闭 / 切换选项卡等结构变化**立即落盘**，编辑内容**防抖 300ms** 落盘。因此无论用哪种方式关闭（点窗口 × / 「文件 → 退出」 / 强杀进程），盘上都是最新状态。
- **关闭程序（点窗口 ×）不询问、默认保存**：临时文件本就连续写在盘上，直接关闭，下次启动原样恢复。
- **关闭未命名选项卡（点其 ×）会弹确认框**：「保存」= 弹出另存为对话框把内容转正为真实文件后关闭；「不保存」= 直接关闭并**删除该临时文件**（`session\` 里对应槽位清掉，下次启动不残留）；「取消」= 留在应用。
- **已保存（有路径）的选项卡**：干净时直接关闭；有未落盘修改时才弹「保存 / 不保存 / 取消」（保存写回原路径，不保存丢弃，取消留在应用）。
- **临时文件位置**（与配置文件同目录，随配置文件路径走）：
  - 前提：`配置文件` 在哪里，`session\` 就在它同目录下（默认 `%APPDATA%\gpui-editor\`）。
  - `session\session.json`：会话清单（版本、活动选项卡下标、按顺序的选项卡列表；未命名选项卡记「槽位名」，已打开文件记绝对路径）。
  - `session\t0`、`t1`…：未命名选项卡的内容槽位文件（LF 规范化，空内容也写空文件；文件型选项卡**不保存正文**，只记路径）。
  - 它在用户目录、**不在仓库里**，不会随 git 提交。
- **范围与限制**：只持久化 / 恢复**主窗口**（拆分 / 拖出的独立窗口不纳入）；已打开文件的**未保存修改不保留**（只记路径，下次重新打开文件本体）；文件缺失时恢复自动跳过该选项卡。
- **恢复优先级**：命令行首个参数是文件时保持既有行为（打开该文件）、跳过会话恢复（显式意图优先）。

## 已知限制与后续方向

- 内容区拖选是整条文本维度的选择；图表 / 图片等富媒体尚未支持选择。
- 编辑区内**查找 / 替换 / 高亮全部匹配**尚未实现（现有侧栏搜索框是「查结果列表」，不替代编辑区内「跳到下一个匹配」）。
- Markdown 预览自定义行内样式（加粗 / 斜体 / 行内代码已解析为样式段预留，尚未逐字着色）；复杂表格与链接点击等富特性待后续增强。
- 更多规划见 `docs/future-work.md`（死依赖已清、README 已对齐；后续为巨函数分解、fixture 收仓、CI 等）。
