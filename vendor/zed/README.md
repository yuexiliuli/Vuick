# vendor/zed —— Zed 上游代码子集（fork）来源与改动记录

> 本目录是从 [zed-industries/zed](https://github.com/zed-industries/zed)（Windows 相关子集）**fork 并整仓携带**的 gpui 组件，作为本工作区成员直接编译。
> 所有 crate 均以 **Apache-2.0** 许可（见 `LICENSE-APACHE`）；依赖 v3.0 许可文本见 https://www.apache.org/licenses/LICENSE-2.0 。

## 来源

- 上游仓库：<https://github.com/zed-industries/zed>
- 上游 rev：`93d7456`（git checkout 对应 `~/.cargo/git/checkouts/zed-a70e2ad075855582/93d7456`）
- 拷贝范围：`crates/` 下本项目需要的 Windows 子集（gpui / gpui_windows / gpui_platform / gpui_macros /
  gpui_shared_string / gpui_util / collections / scheduler / sum_tree / refineable(+derive_refineable) /
  http_client / ztracing / ztracing_macro）。
- fork 动机：GPU 直通（YUV 着色器 / D3D11 设备暴露 / 外部纹理）需改 `gpui`/`gpui_windows`，改为仓内 fork 可随意改。

## 相对上游的改动（按 crate）

凡源/构建/配置文件被改动者，文件头部均有 `[gpui-editor fork]` 显著标注（满足 Apache-2.0 §4(b)）。

| crate | 改动文件 | 改动内容 |
|---|---|---|
| gpui | `src/scene.rs`、`src/window.rs`、`resources/windows/gpui.rc`、`Cargo.toml` | GPU 直通 / YUV 着色器相关宿主改动 + Windows 资源清单调整 + 依赖裁剪 |
| gpui_windows | `src/directx_renderer.rs`、`src/gpui_windows.rs`、`src/platform.rs`、`src/util.rs`、`src/shaders.hlsl`、`build.rs`、`Cargo.toml` | D3D11 硬件设备暴露、外部纹理、着色器增强（外部解除锁，786 行增量） |
| gpui_platform | `src/gpui_platform.rs`、`Cargo.toml` | 平台 trait 再导出整理 + 依赖裁剪 |
| 其余 11 crate | `Cargo.toml` | 依赖/feature 裁剪与 workspace 对齐（无逻辑改动） |

## 对本目录做改动时的纪律

- 若从上游同步更新，请整目录对比 `git diff --no-index <checkout>/crates/<x> <vendor>/crates/<x>`，把改动合并进对应文件并在 README 本表登记。
- 新增改动的文件，务必追加/保留头部 `[gpui-editor fork]` 标注。
- 许可证：保持 Apache-2.0；不得删除 `LICENSE-APACHE`（各 crate 内的 `LICENSE-APACHE` 是指针，指向本目录 `LICENSE-APACHE`）。
