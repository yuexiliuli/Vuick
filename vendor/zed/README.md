# vendor/zed —— Zed 上游代码子集（fork）来源与改动记录

> 本目录是从 [zed-industries/zed](https://github.com/zed-industries/zed)（Windows 子集 + 2026-11 Phase 2 补入的 Linux 平台后端）**fork 并整仓携带**的 gpui 组件，作为本工作区成员直接编译。
> 所有 crate 均以 **Apache-2.0** 许可（见 `LICENSE-APACHE`）；依赖 v3.0 许可文本见 https://www.apache.org/licenses/LICENSE-2.0 。

## 来源

- 上游仓库：<https://github.com/zed-industries/zed>
- 上游 rev：`93d7456`（git checkout 对应 `~/.cargo/git/checkouts/zed-a70e2ad075855582/93d7456`）
- 拷贝范围：`crates/` 下本项目需要的子集（gpui / gpui_windows / gpui_platform / gpui_macros /
  gpui_shared_string / gpui_util / collections / scheduler / sum_tree / refineable(+derive_refineable) /
  http_client）；2026-11 Phase 2 补入 Linux 平台后端 `gpui_linux`（x11/wayland 双后端）与
  `gpui_wgpu`（wgpu_renderer + cosmic_text_system），均为上游 rev 93d7456 原样拷贝（无逻辑改动）。
- fork 动机：GPU 直通（YUV 着色器 / D3D11 设备暴露 / 外部纹理）需改 `gpui`/`gpui_windows`，改为仓内 fork 可随意改。

## 相对上游的改动（按 crate）

凡源/构建/配置文件被改动者，文件头部均有 `[vuick fork]` 显著标注（满足 Apache-2.0 §4(b)）。

| crate | 改动文件 | 改动内容 |
|---|---|---|
| gpui | `src/scene.rs`、`src/window.rs`、`resources/windows/gpui.rc`、`Cargo.toml` | GPU 直通 / YUV 着色器相关宿主改动 + Windows 资源清单调整 + 依赖裁剪 |
| gpui_windows | `src/directx_renderer.rs`、`src/gpui_windows.rs`、`src/platform.rs`、`src/util.rs`、`src/shaders.hlsl`、`build.rs`、`Cargo.toml` | D3D11 硬件设备暴露、外部纹理、着色器增强（外部解除锁，786 行增量） |
| gpui_platform | `src/gpui_platform.rs`、`Cargo.toml` | 平台 trait 再导出整理 + 依赖裁剪；2026-11 Phase 2 → `Cargo.toml` 归还 Linux 分支（`gpui_linux`/`gpui_wgpu` 目标依赖 + `wayland`/`x11` feature 转发，Windows 相关保持原样） |
| gpui_linux | `Cargo.toml` | 2026-11 Phase 2 新增：上游 rev 93d7456 原样拷贝（x11/wayland 双后端），仅加 `[vuick fork]` 头部标注，无逻辑改动 |
| gpui_linux | `src/linux/x11/window.rs` | fork 分歧修正（2026-11-22）：x11 建窗按窗口透明度选 visual——不透明窗口取 depth-24 opaque visual，仅真透明窗口才取 ARGB transparent visual；根因：ARGB（depth-32）透明 visual 需合成管理器，无合成器（Xvfb/裸 X/无 WM）时 ARGB 窗恒黑（实测 winmean=0→251.99）。配套：`WindowParams` 新增 `window_background` 字段（gpui `src/platform.rs`）透传创建期背景透明度；wgpu surface `transparent` flag 跟随。 |
| gpui_wgpu | `Cargo.toml` | 2026-11 Phase 2 新增：上游 rev 93d7456 原样拷贝（wgpu_renderer + cosmic_text_system），仅加 `[vuick fork]` 头部标注，无逻辑改动 |
| 其余 9 crate | `Cargo.toml` | 依赖/feature 裁剪与 workspace 对齐（无逻辑改动） |

> 2026-10-19：`ztracing` / `ztracing_macro` 已移除。二者仅是默认空转的 tracing 封装（无操作宏
> `#[instrument]` 恒等透传），唯一使用者 `sum_tree` 已同步移除探针属性与依赖，运行行为零变化。

> 2026-11 Phase 2：自本机离线 git 对象库抽入 upstream rev 93d7456 的 `gpui_linux`（29 文件）与
> `gpui_wgpu`（12 文件），纯搬移、逐字节 hash 校验对上（含 `LICENSE-APACHE` 指针文件，存为常规文件与
> 既有的 20 字节指针一致）；`gpui_linux` 库级 `#![cfg(any(target_os = "linux", target_os = "freebsd"))]`
> 在 Windows 上是空 crate，`gpui_wgpu` 无平台门控（Windows 上可正常编译）。

## 对本目录做改动时的纪律

- 若从上游同步更新，请整目录对比 `git diff --no-index <checkout>/crates/<x> <vendor>/crates/<x>`，把改动合并进对应文件并在 README 本表登记。
- 新增改动的文件，务必追加/保留头部 `[vuick fork]` 标注。
- 许可证：保持 Apache-2.0；不得删除 `LICENSE-APACHE`（各 crate 内的 `LICENSE-APACHE` 是指针，指向本目录 `LICENSE-APACHE`）。
