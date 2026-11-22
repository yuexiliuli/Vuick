//! pdfium 引导：DLL 定位 / sha256 校验 / 内嵌解压 / 动态库路径缓存。
//!
//! 定位优先级（**只用带版本号的文件**，不退回裸 `pdfium.dll`）：
//! 1. 环境变量 `VUICK_PDFIUM`（逃生口，支持任意文件名）
//! 2. 可执行文件同目录 `<版本号>.dll`
//! 3. 项目 `resources/pdfium/` `<版本号>.dll`
//! 4. 内嵌到 exe 的 pdfium（`embed_pdfium` cfg）解出到临时目录
//!
//! `PDFIUM_PATH` 进程级缓存在 `OnceLock` 里（只定位一次）。根模块
//! `shared_worker()` 经 `pdfium_dll_path()`（`pub(crate)`）取路径。

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use sha2::{Digest, Sha256};

/// 进程级单例：pdfium 动态库路径（只定位一次）。
static PDFIUM_PATH: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// pdfium 动态库平台文件名（与 `resources/pdfium/` 内实际文件、`build.rs` 的
/// `pdfium_lib_name()` 保持一致——Windows 保留版本号 `.dll`，非 Windows 走系统常见命名）。
///
/// Windows 用**带版本号**的文件名（`pdfium-<版本>.dll`）替代裸 `pdfium.dll`：
/// - 一眼可辨当前内置的 pdfium 快照版本；
/// - 升级 pdfium 时新旧文件可并存，随时回滚，不用覆盖。
///
/// 非 Windows（`libpdfium.so` / `libpdfium.dylib`）本仓**不供给**对应文件——缺失时走现有
/// 「仅磁盘候选/报错」路径（无内嵌兜底），编译不受影响，与 build.rs 静默跳过逻辑自洽。
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

/// 内嵌的 pdfium 字节（仅当 `build.rs` 探测到 dll 存在时启用 `embed_pdfium` cfg）。
///
/// `build.rs` 已用 **zstd level 19** 压缩（6.9MiB→~2.9MiB），这里嵌入的是**压缩载荷**；
/// 运行期由 [`extract_embedded_pdfium`] 一次性解压到临时目录再动态加载
/// （Windows 的 `libloading` 必须从磁盘加载 dll，无法直接内存加载）。
/// 若本机没有该 dll（未触发 `embed_pdfium`），则退化为仅磁盘查找。
///
/// 路径由 `build.rs` 通过 `cargo:rustc-env=PDFIUM_ZST_PATH` 注入（指向 OUT_DIR 内压缩中间文件）。
#[cfg(embed_pdfium)]
pub(crate) const EMBEDDED_PDFIUM: &[u8] = include_bytes!(env!("PDFIUM_ZST_PATH"));

/// 原始（未压缩）dll 字节数，由 `build.rs` 注入：运行期解压的容量预算与临时文件复用判据。
#[cfg(embed_pdfium)]
pub(crate) static PDFIUM_RAW_SIZE: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| env!("PDFIUM_RAW_SIZE").parse::<usize>().unwrap());

/// 原始 dll 的 sha256（hex，64 字符），由 `build.rs` 注入（构建期对 resources/ 内原 dll
/// 计算）：复用临时 dll 前比对磁盘 sha256，防「同长但内容被种植」的 DLL 投毒（长度
/// 快拒之外的第二道闸，见 [`extract_embedded_pdfium`]）。
#[cfg(embed_pdfium)]
const PDFIUM_RAW_SHA256_HEX: &str = env!("PDFIUM_RAW_SHA256");

/// 计算字节集的 sha256（hex 小写）。纯函数供单测：同字节集恒等、异字节集必异。
#[cfg_attr(not(embed_pdfium), allow(dead_code))]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 临时 dll 是否可复用（纯函数，裁决 = 「复用 / 走重解压覆盖」）：
/// **先比长度**（明显不同 size 免做全量 hash 读），同长**再比 sha256** 防「同长不同内容」
/// 的种植。供 [`extract_embedded_pdfium`] 复用判据与单测直接调用（不依赖 embed cfg/磁盘）。
#[cfg_attr(not(embed_pdfium), allow(dead_code))]
pub(crate) fn pdfium_reuse_decision(
    disk_len: u64,
    disk_sha256_hex: &str,
    expected_len: u64,
    expected_sha256_hex: &str,
) -> bool {
    disk_len == expected_len && disk_sha256_hex == expected_sha256_hex
}

/// 在临时目录解出内嵌的 pdfium 并返回其路径。
///
/// 文件命名为 `vuick/pdfium-<版本>.dll`（仅首次写入；内容与 `include_bytes!`
/// 解压后一致时直接复用，退出后由临时目录清理，不污染项目目录）。
/// 复用前除**长度快拒**外还比对 **sha256**：磁盘文件被「同长不同内容」种植时判定不复用，
/// 落到下方重解压覆盖路径（覆盖被种植文件）。
/// 返回 `None` = 无法提供**已校验**的 dll（复用 hash 不匹配且覆盖写失败，如种植文件被
/// 设只读/占用）：此时**绝不返回**可能被种植的旧文件路径——宁可让 locate_dll 走其它
/// 候选或报「找不到 pdfium」，也不把 hash 校验不过的文件落回加载面（评审 NIT#1）。
#[cfg(embed_pdfium)]
pub(crate) fn extract_embedded_pdfium() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("vuick");
    let path = dir.join(PDFIUM_VERSIONED_NAME);
    // 已存在且长度等于原始 dll 大小 → 再比 sha256 才复用（避免每次启动重解压 ~3MB writes；
    // 长度快拒在前，明显不同 size 的文件不做全量 hash 读）。
    let reuse = path.is_file()
        && {
            let len = path.metadata().map(|m| m.len()).unwrap_or(0);
            len == *PDFIUM_RAW_SIZE as u64
                && std::fs::read(&path)
                    .map(|disk| {
                        pdfium_reuse_decision(
                            len,
                            &sha256_hex(&disk),
                            *PDFIUM_RAW_SIZE as u64,
                            PDFIUM_RAW_SHA256_HEX,
                        )
                    })
                    .unwrap_or(false)
        };
    if reuse {
        return Some(path);
    }
    let _ = std::fs::create_dir_all(&dir);
    let raw = zstd::bulk::decompress(EMBEDDED_PDFIUM, *PDFIUM_RAW_SIZE)
        .expect("解压内嵌 pdfium 失败");
    // 覆盖写失败（含被种植文件被设只读/占用）→ 返回 None：被种植的旧 dll 既不可复用、
    // 也不再被返回，绝不落回加载面（宁可报错/走其它候选，不当沉默误装载）。
    std::fs::write(&path, raw).ok()?;
    Some(path)
}

/// 定位 pdfium 动态库（按优先级，**只用带版本号的文件**，不退回裸 `pdfium.dll`）：
/// 1. 环境变量 `VUICK_PDFIUM`：绝对路径直接指定（最优先，逃生口；支持任意文件名）
/// 2. 可执行文件同目录：`<版本号>.dll`（旁置分发/就地升级，可覆盖内嵌版）
/// 3. 项目 `resources/pdfium/`：`<版本号>.dll`（开发树自带）
/// 4. 内嵌到 exe 的 pdfium（`embed_pdfium` cfg）：解出到临时目录（单文件分发兜底）
fn locate_dll() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("VUICK_PDFIUM") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Ok(pb);
        }
    }
    #[allow(unused)]
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        candidates.push(dir.join(PDFIUM_VERSIONED_NAME));
    }
    // 用**编译期** manifest（pdf_core 自身目录）而非运行期 `CARGO_MANIFEST_DIR`：
    // 运行期该变量属于「运行中的包」——`cargo run` 时是根包（仓库根），../../ 会再多上一级
    // 到 workspace 层，找不到 resources/pdfium；编译期恒等于 crates/pdf_core，../../ = 仓库根。
    // （与 build.rs 的 `../../resources` 上溯逻辑同源。）
    let manifest = env!("CARGO_MANIFEST_DIR");
    candidates.push(
        PathBuf::from(manifest)
            .join("../../resources/pdfium")
            .join(PDFIUM_VERSIONED_NAME),
    );
    #[cfg(embed_pdfium)]
    if let Some(p) = extract_embedded_pdfium() {
        candidates.push(p);
    }
    #[cfg(not(embed_pdfium))]
    let _ = &candidates; // 无内嵌：candidates 仅磁盘候选

    candidates
        .into_iter()
        .find(|c| c.is_file())
        .ok_or_else(|| anyhow::anyhow!(
            "无法找到 pdfium 动态库 {PDFIUM_VERSIONED_NAME}：\
             请把它放在可执行文件旁或 resources/pdfium/，或用 VUICK_PDFIUM=<路径> 指定"
        ))
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
