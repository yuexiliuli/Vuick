//! pdfium 引导：DLL 定位 / 动态库路径缓存。
//!
//! 定位优先级（**只用带版本号的文件**，不退回裸 `pdfium.dll`）：
//! 1. 环境变量 `VUICK_PDFIUM`（逃生口，支持任意文件名）
//! 2. 可执行文件 `bin\pdf\` 子目录（**分发布局**：exe 在 dist 根、pdfium 在 `dist\bin\pdf\`）
//! 3. 可执行文件同目录 `<版本号>.dll`（旁置分发/就地升级）
//! 4. 项目 `resources/pdfium/` `<版本号>.dll`（开发树自带）
//!
//! 2026-11-22 起 **pdfium 不再内嵌进 exe**（体积减小；运行只走磁盘候选）：原 `embed_pdfium`
//! 内嵌解压路径已整体移除（build.rs 不再压缩注入、本文件不再有内嵌字节/解压）。
//!
//! `PDFIUM_PATH` 进程级缓存在 `OnceLock` 里（只定位一次）。根模块
//! `shared_worker()` 经 `pdfium_dll_path()`（`pub(crate)`）取路径。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;
// sha2 仅剩单测侧 dormancy 纯工具使用（内嵌校验已随 2026-11-22 去内嵌移除）。
#[cfg(test)]
use sha2::{Digest, Sha256};

/// 进程级单例：pdfium 动态库路径（只定位一次）。
static PDFIUM_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// pdfium 动态库平台文件名（与 `resources/pdfium/` 内实际文件保持一致——Windows 保留版本号
/// `.dll`，非 Windows 走系统常见命名）。
///
/// Windows 用**带版本号**的文件名（`pdfium-<版本>.dll`）替代裸 `pdfium.dll`：
/// - 一眼可辨当前内置的 pdfium 快照版本；
/// - 升级 pdfium 时新旧文件可并存，随时回滚，不用覆盖。
///
/// 非 Windows（`libpdfium.so` / `libpdfium.dylib`）本仓**不供给**对应文件——缺失时走现有
/// 「仅磁盘候选/报错」路径（已无内嵌兜底），编译不受影响。
///
/// 跨可见性（模块重组）：`parser/tests.rs` 的 DLL 定位用例引用 → 升 `pub(crate)`。
#[cfg(windows)]
pub(crate) const PDFIUM_VERSIONED_NAME: &str = "pdfium-154.0.8035.0.dll";
#[cfg(target_os = "linux")]
pub(crate) const PDFIUM_VERSIONED_NAME: &str = "libpdfium.so";
#[cfg(target_os = "macos")]
pub(crate) const PDFIUM_VERSIONED_NAME: &str = "libpdfium.dylib";
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) const PDFIUM_VERSIONED_NAME: &str = "pdfium-154.0.8035.0.dll";

/// 计算字节集的 sha256（hex 小写）。纯函数：同字节集恒等、异字节集必异。
/// 内嵌解压路径已移除（2026-11-22），本函数仅作 dormancy 纯工具（K24）供单测调用。
#[cfg(test)]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 临时 dll 是否可复用（纯函数，裁决 = 「复用 / 走重解压覆盖」）：
/// **先比长度**（明显不同 size 免做全量 hash 读），同长**再比 sha256** 防「同长不同内容」
/// 的种植。原供内嵌解压复用判据（embed 已移除，2026-11-22）；保留为 dormancy 纯工具（K24）
/// 供单测。
#[cfg(test)]
pub(crate) fn pdfium_reuse_decision(
    disk_len: u64,
    disk_sha256_hex: &str,
    expected_len: u64,
    expected_sha256_hex: &str,
) -> bool {
    disk_len == expected_len && disk_sha256_hex == expected_sha256_hex
}

/// 定位 pdfium 动态库（按优先级，**只用带版本号的文件**，不退回裸 `pdfium.dll`）：
/// 1. 环境变量 `VUICK_PDFIUM`：绝对路径直接指定（最优先，逃生口；支持任意文件名）
/// 2. 可执行文件 `bin\pdf\`：`<版本号>.dll`（分发布局：exe 在 dist 根、pdfium 在 `dist\bin\pdf\`）
/// 3. 可执行文件同目录：`<版本号>.dll`（旁置分发/就地升级）
/// 4. 项目 `resources/pdfium/`：`<版本号>.dll`（开发树自带）
/// （2026-11-22 起无内嵌兜底——单文件分发不再可用，见模块头。）
fn locate_dll() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("VUICK_PDFIUM") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Ok(pb);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    disk_candidates(exe_dir.as_deref(), env!("CARGO_MANIFEST_DIR"))
        .into_iter()
        .find(|c| c.is_file())
        .ok_or_else(|| anyhow::anyhow!(
            "无法找到 pdfium 动态库 {PDFIUM_VERSIONED_NAME}：\
             请把它放在可执行文件的 bin\\pdf\\ 子目录、可执行文件旁或 resources/pdfium/，\
             或用 VUICK_PDFIUM=<路径> 指定"
        ))
}

/// pdfium 磁盘候选路径集合（**可纯测**，顺序即优先级，与 [`locate_dll`] 一致）：
/// `[exe_dir/bin/pdf/<版本>, exe_dir/<版本>, resources/pdfium/<版本>]`。
///
/// `manifest` 用**编译期** `CARGO_MANIFEST_DIR`（pdf_core 自身目录）而非运行期变量：
/// 运行期该变量属于「运行中的包」——`cargo run` 时是根包（仓库根），../../ 会再多上一级
/// 到 workspace 层，找不到 resources/pdfium；编译期恒等于 crates/pdf_core，../../ = 仓库根。
/// （与旧 build.rs 的 `../../resources` 上溯逻辑同源。）
/// `pub(crate)`：供 `parser/tests.rs` 的候选顺序用例引用（同 [`PDFIUM_VERSIONED_NAME`]）。
pub(crate) fn disk_candidates(exe_dir: Option<&Path>, manifest: &str) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("bin").join("pdf").join(PDFIUM_VERSIONED_NAME));
        candidates.push(dir.join(PDFIUM_VERSIONED_NAME));
    }
    candidates.push(
        PathBuf::from(manifest)
            .join("../../resources/pdfium")
            .join(PDFIUM_VERSIONED_NAME),
    );
    candidates
}

/// 取进程级缓存的 pdfium 路径（只定位一次）。
/// 跨簇可见性（模块重组）：根模块 `shared_worker()` 调用 → 升 `pub(crate)`。
pub(crate) fn pdfium_dll_path() -> Result<&'static PathBuf> {
    let cached = PDFIUM_PATH.get_or_init(|| locate_dll().map_err(|e| e.to_string()));
    match cached {
        Ok(p) => Ok(p),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}
