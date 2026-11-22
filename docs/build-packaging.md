> 本文内容原属 README.md 说明书部分，重写后拆分至此。

# 构建、打包与分发

## 构建与运行

**视频功能依赖 FFmpeg 开发环境**（`ffmpeg-sys-next` 链接期需要）：
- `FFMPEG_DIR` → 指向含 `include/` `lib/` `bin/` 的 FFmpeg **win64 shared** 构建（gyan / BtbN，9.x）；
- `LIBCLANG_PATH` → 指向含 `libclang.dll` 的目录（bindgen 必需，装 LLVM 即得）。

一键装环境：`pwsh scripts/setup-ffmpeg.ps1`（下载 / 配置提示、写用户环境变量，新终端生效）。
跳过视频功能可 `cargo build --workspace --no-default-features --exclude media_ffmpeg`（不编译
`src/video/`，也不构建 `media_ffmpeg` cdylib——否则默认 workspace 构建仍需要 FFmpeg 环境）。

PDF 预览依赖 pdfium 动态库（**不再内嵌进 exe**）：运行时按磁盘候选路径加载，分发布局优先 exe 的 `bin\pdf\pdfium-154.0.8035.0.dll`，开发树可放 `resources/pdfium/pdfium-154.0.8035.0.dll`（候选顺序与加载逻辑见 `crates/pdf_core/src/parser/pdfium.rs`）。打包时 `scripts/package.ps1` 会自动拷入 `dist\bin\pdf\`（缺则先拷 `resources\pdfium\` 同名 dll，仍缺则从 https://github.com/bblanchon/pdfium-binaries/releases 自动下载）。

```bash
# 运行（开发）
cargo run

# 发布（Release profile 已配 opt-level=z / LTO / strip / panic=abort）
cargo build --release

# 分发打包（release exe + media_ffmpeg.dll + FFmpeg 运行库 + pdfium → dist\）
# 自动供给默认开：FFMPEG_DIR 缺/不完整会自建 FFmpeg 瘦包、pdfium 缺会自动下载，可 -Skip* 关闭
pwsh scripts/package.ps1

# 全量测试
cargo test

# Lint
cargo clippy --workspace --all-targets
```

## 测试 fixture 说明

PDF / EPUB / 图片 / 大文档测试的 fixture **已提交至仓库**（`fixtures/`，不入 `target/`），fresh clone 后 `cargo test` 直接全绿，无需手动放置：

| fixture | 说明 |
| --- | --- |
| `fixtures/pdf测试.pdf` | 87 页文本型 PDF（长文档 / 遍历 / 搜索用例；长文遍历用例共用此份） |
| `fixtures/epub测试.epub` | 6 图 EPUB |
| `fixtures/img测试.png` | 实为 580x773 JPEG（magic 探测） |
| `fixtures/大文档测试.rs` / `fixtures/大文档测试2.md` | 6.4MB 大文档（编辑器 / 预览交互与 `BIGDOC_BENCH=1` 延迟基准） |

> 超大文档档位（`大文档测试.md` / `大文档测试23MB*` / `大文档测试116MB.txt` / `大文档测试126MB.txt` 等）体积过大不入库，仅供 `#[ignore]` 手动 / 基准用例按需放置至 `fixtures/`。`cargo test` 的回归护栏覆盖：滚动条拖动、WYSIWYG 实时滚动、Markdown 预览滚动、编辑器远跳 / 延迟滚动、CJK 字符边界点击、撤销 / 重做、全选替换等。

## 发布 / 打包分发

视频解码内核已拆成独立 cdylib（`media_ffmpeg` 工作区成员，产物 `media_ffmpeg.dll`）：
主 exe **不静态导入**任何 FFmpeg dll，只在打开视频时经 libloading **运行时**加载它。
所以：
- **exe 缺 `media_ffmpeg.dll` / FFmpeg 运行库也能正常启动**——不开视频照常使用全部其它功能，开视频时在窗口内提示「缺少视频运行库」；
- 要让视频功能可用，分发时必须把 `media_ffmpeg.dll` 与全部 FFmpeg dll 放进 exe 的 `bin\ffmpeg\` 子目录（运行时候选 = exe 旁 → `bin\ffmpeg\`，见 `crates/media_io/src/bridge.rs`）。一键打包：

```bash
# 构建 release 并导出「exe + bin\ffmpeg\（media_ffmpeg.dll + FFmpeg 运行库）+ bin\pdf\（pdfium）」到 dist\
# 自动供给默认开：FFMPEG_DIR 缺/不完整时用 scripts/build-slim-ffmpeg.sh 自建 B1 瘦包（需 MSYS2 UCRT64 bash）；
#   bin\pdf\ 缺 pdfium 时先拷 resources\pdfium\、仍缺则从 bblanchon/pdfium-binaries 下载。可
#   -SkipFfmpegProvision / -SkipPdfiumProvision 关闭，-PdfiumUrl / -FfmpegPrefix 覆盖供给参数。
pwsh scripts/package.ps1
```

## 分发布局与 DLL 清单

`scripts/package.ps1` 产出到 `dist\`：

```
dist\
├─ vuick.exe                          （dist 根）
├─ bin\ffmpeg\                        （media_ffmpeg.dll + FFmpeg 运行库）
│   ├─ media_ffmpeg.dll
│   ├─ avcodec-63.dll / avformat-63.dll / avutil-61.dll / swresample-7.dll / swscale-10.dll
├─ bin\pdf\pdfium-154.0.8035.0.dll    （pdfium，不再内嵌 exe，走磁盘分发）
├─ THIRD_PARTY_NOTICES.md + third_party\
   （不含任何 *.lib：导入库仅 build 期用，脚本装配后断言防混入）
```

| 文件 | 来源 | 说明 |
| --- | --- | --- |
| `vuick.exe` | 本仓库 `target\release\` | 主程序（release；debug 依赖调试 CRT 不入包） |
| `bin\ffmpeg\media_ffmpeg.dll` | 本仓库 `target\release\`（cdylib） | 视频/音乐解码内核：主 exe 打开视频时经 libloading **运行时**加载它 |
| `bin\ffmpeg\avcodec-63.dll` | `FFMPEG_DIR\bin` | 音视频编解码（按 media_ffmpeg.dll 实际导入表驱动，只带引用件） |
| `bin\ffmpeg\avformat-63.dll` | `FFMPEG_DIR\bin` | 容器格式（MP4 / MKV / AVI / MOV…）解封装 |
| `bin\ffmpeg\avutil-61.dll` | `FFMPEG_DIR\bin` | 通用工具库（内存 / 数学 / 字典…） |
| `bin\ffmpeg\swscale-10.dll` | `FFMPEG_DIR\bin` | 图像缩放与像素格式转换（→ BGRA 上屏） |
| `bin\ffmpeg\swresample-7.dll` | `FFMPEG_DIR\bin` | 音频重采样（→ F32 立体声） |
| `bin\pdf\pdfium-154.0.8035.0.dll` | `resources\pdfium\` 或 bblanchon/pdfium-binaries | PDF 渲染（**不再内嵌 exe**，磁盘分发，运行时按 `bin\pdf\` → exe 旁 → `resources\pdfium\` 顺序候选） |

缺其中任何 / 全部 → **exe 照常启动**，打开视频/PDF 时窗口内提示缺库。
`scripts/package.ps1` 按 media_ffmpeg.dll 的**实际导入表**只拷它引用的 FFmpeg dll（B1 shared 默认 5 件；
B2 静态链形态仅 `bin\ffmpeg\media_ffmpeg.dll` 一件），手工分发请以**与编译期同一 FFmpeg 构建**
（版本号一致，防 ABI 错位）为准补全。

- **必须用 release**：debug 版还依赖 `vcruntime140d.dll` / `ucrtbased.dll` 等调试 CRT，他人机器也没有；
- 把 `dist\` 整个目录（或打成 zip）发给对方，**exe 必须与 `bin\ffmpeg\` / `bin\pdf\` 子目录一起分发**；
- 其余依赖（`VCRUNTIME140.dll`、`api-ms-win-crt-*`、`d3dcompiler_47.dll`、`icuuc.dll`）均为
  Windows 10/11 系统自带，无需打包；对方是精简版 / 旧版系统缺 `VCRUNTIME140.dll` 时，安装
  「Microsoft Visual C++ 2015–2022 Redistributable (x64)」即可；
- DLL 须与编译期是**同一个** FFmpeg 构建（脚本直接从 `FFMPEG_DIR\bin` 取，版本号一致，不会 ABI 错位）。

## 许可证与分发合规

本体代码许可、FFmpeg / zstd / pdfium 等第三方组件许可与专利口径的完整明细，详见 `docs/licensing.md`；分发包（`scripts/package.ps1` 产出）内随包附 `THIRD_PARTY_NOTICES.md` 与 `third_party/` 许可原文，分发时请随包原样保留。
