> 本文内容原属 README.md 说明书部分，重写后拆分至此。

# 构建、打包与分发

## 构建与运行

**视频功能依赖 FFmpeg 开发环境**（`ffmpeg-sys-next` 链接期需要）：
- `FFMPEG_DIR` → 指向含 `include/` `lib/` `bin/` 的 FFmpeg **win64 shared** 构建（gyan / BtbN，9.x）；
- `LIBCLANG_PATH` → 指向含 `libclang.dll` 的目录（bindgen 必需，装 LLVM 即得）。

一键装环境：`pwsh scripts/setup-ffmpeg.ps1`（下载 / 配置提示、写用户环境变量，新终端生效）。
跳过视频功能可 `cargo build --workspace --no-default-features --exclude media_ffmpeg`（不编译
`src/video/`，也不构建 `media_ffmpeg` cdylib——否则默认 workspace 构建仍需要 FFmpeg 环境）。

PDF 预览依赖 pdfium 动态库：前往 https://github.com/bblanchon/pdfium-binaries/releases 下载，解压得到 `pdfium.dll`，重命名并放于 `resources/pdfium/pdfium-154.0.8035.0.dll`（构建时若探测到会**内嵌进 exe**，分发免带）。

```bash
# 运行（开发）
cargo run

# 发布（Release profile 已配 opt-level=z / LTO / strip / panic=abort）
cargo build --release

# 分发打包（release exe + media_ffmpeg.dll + FFmpeg 运行库 → dist\vuick-win64\）
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
- 要让视频功能可用，分发时必须把 `media_ffmpeg.dll` 与全部 FFmpeg dll 和 exe **放到同一目录**（清单见下方「视频组件所需 DLL」）。一键打包：

```bash
# 构建 release 并导出「exe + media_ffmpeg.dll + 全部 FFmpeg 运行库」到 dist\vuick-win64\
# 脚本自动校验：exe 无 FFmpeg 静态导入、media_ffmpeg.dll 所需的 FFmpeg dll 全部在包内
pwsh scripts/package.ps1
```

## 视频组件所需 DLL（与 exe 同目录）

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
- 把 `dist\vuick-win64\` 整个目录（或打成 zip）发给对方，**exe 必须与这些 dll 同目录**；
- 其余依赖（`VCRUNTIME140.dll`、`api-ms-win-crt-*`、`d3dcompiler_47.dll`、`icuuc.dll`）均为
  Windows 10/11 系统自带，无需打包；对方是精简版 / 旧版系统缺 `VCRUNTIME140.dll` 时，安装
  「Microsoft Visual C++ 2015–2022 Redistributable (x64)」即可；
- DLL 须与编译期是**同一个** FFmpeg 构建（脚本直接从 `FFMPEG_DIR\bin` 取，版本号一致，不会 ABI 错位）；
- **PDF 无需额外分发**：pdfium dll 构建时经 zstd-19 压缩（6.9MiB→~2.9MiB）后内嵌进 exe（`embed_pdfium`），运行时一次性解压到临时目录再自动加载。

## 许可证与分发合规

本体代码许可、FFmpeg / zstd / pdfium 等第三方组件许可与专利口径的完整明细，详见 `docs/licensing.md`；分发包（`scripts/package.ps1` 产出）内随包附 `THIRD_PARTY_NOTICES.md` 与 `third_party/` 许可原文，分发时请随包原样保留。
