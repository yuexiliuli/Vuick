> 本文内容原属 README.md 说明书部分，重写后拆分至此。

# 架构与技术

## 技术栈

| 类别 | 选型 |
| --- | --- |
| 语言 / UI | Rust · GPUI  |
| 文件对话框 | `rfd`（系统原生打开 / 保存对话框） |
| 文本 / 编辑 | `ropey`（缓冲区）· `regex` · `memchr` · `unicode-segmentation` · `unicode-bidi` · `rustc-hash` |
| 语法高亮 | `syntect`（解析）→ 自研 `GitHubTheme` / `SemanticStyleResolver` |
| 数学排版 | `ratex-parser` / `ratex-layout` / `ratex-render`（KaTeX 字体，纯 CPU 光栅） |
| 异步 | `futures`（大文档「分块读盘·渐进装配」通道） |
| 编码 | `encoding_rs`（GBK/GB18030/UTF-16 等非 UTF-8 解码；保存按各选项卡源编码回写） |
| PDF | `pdfium-render` |
| EPUB / 压缩 | `roxmltree` · `zip` |
| 图像 | `kimg`（纯 Rust 图像编解码核心：PNG/JPEG/WebP/GIF/BMP/ICO/PNM/TIFF 解码，PNG/GIF 编码；[sdcb/SimdCodecs](https://github.com/sdcb/SimdCodecs) 的 Rust 重写版）|
| 视频 | `ffmpeg-next`（FFmpeg 9.0 解码 / 重采样）· `cpal`（音频输出） |
| 序列化 | `serde` · `serde_json` |
| 测试 | GPUI `test-support`（`cargo test`；shell 全量 ~80s、271 passed，测试默认隔离落 `%TEMP%`） |

### 外部二进制依赖下载

| 依赖 | 用途 | 下载地址 |
| --- | --- | --- |
| PDFium | PDF 栅格化（`resources/pdfium/pdfium-154.0.8035.0.dll`，`crates/pdf/build.rs` 内嵌） | [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)（发布包取 `pdfium-win-x64.tgz`） |
| FFmpeg 9.0.2（LGPL 瘦化构建） | 视频解码 / 重采样（`media_ffmpeg.dll`，运行时动态加载） | 源码（跨平台通用编译入口）：<https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz>；Git 镜像 [github.com/FFmpeg/FFmpeg](https://github.com/FFmpeg/FFmpeg)（分支 `n9.0.2`）—— Windows 开发期直接用预编译共享版：<https://www.gyan.dev/ffmpeg/builds/>（`ffmpeg-release-full-shared`，LGPL）或 [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases)（`win64-gpl-shared`）；正式分发用本仓 `scripts/build-slim-ffmpeg*.sh` 自建瘦共享包 |

## UI 组件库

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

## 目录结构

> 注意：仓库现已拆分为**多 workspace crate**（`crates/`：`shell` 应用外壳 / `editor` 代码编辑器 / `media_ffmpeg` 视频解码内核（独立 cdylib）/ `video` 播放器 / `image_viewer` 图片查看 / `pdf` / `epub` / `music` 音乐播放器 / `markdown` 解析 / `markdown_wysiwyg` 实时预览 / `syntax` 高亮 / `toc` / `viewer` 共享查看器几何 / `ui` 组件库）。下方旧 `src/` 目录树为早期单二进制布局，**除路径前缀外职责描述仍成立**：`src/app` ≈ `crates/shell/src/app`、`src/editor` ≈ `crates/editor`、`src/pdf` ≈ `crates/pdf`，依此类推。

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
