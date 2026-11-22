//! 构建期元数据注入：「帮助 → 关于」面板需要 git 提交 / Rust 工具链 / gpui 版本等
//! 编译期不可得的信息，经 `cargo:rustc-env` 在构建期打入 `env!("VUICK_*")` 可读的常量。
//!
//! 容错纪律（K42 输入安全面）：**任何一步失败都回退空串 / "unknown"，绝不 panic 崩构建**
//! ——git 不在 PATH、工作树不是 git 仓库、文件缺失都是正常工况。消费方
//! （`crates/shell/src/app/about.rs`）自行对空值做 unknown 兜底展示。
//! 路径一律由 `CARGO_MANIFEST_DIR` 推算（仓库根 = shell 上两级），禁写死绝对路径。

use std::path::PathBuf;
use std::process::Command;

/// 仓库根：shell crate 位于 `crates/shell`，上两级即仓库根。
fn repo_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
        .join("..")
        .join("..")
}

/// 执行命令并取 stdout 首行 trim；退出码非 0 / 无输出 → `None`（不 panic）。
fn first_line_ok(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// 从 Cargo.toml 文本解析 `version = "…"` 字段首值（字符串级解析，不引 toml 依赖；
/// 首个 `version = ` 即 `[package]` 版本字段）。读不到 → `None`。
fn read_version_field(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let v = rest.trim().trim_matches('"').trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// 从 Cargo.lock 文本提取 `zed?rev=<40hex>` 的短 rev（前 7 位）。找不到 / 不足 7 位 →
/// `None`。
fn extract_zed_rev(text: &str) -> Option<String> {
    for line in text.lines() {
        if line.contains("zed?rev=")
            && let Some(start) = line.find("rev=")
        {
            let hex: String = line[start + "rev=".len()..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if hex.len() >= 7 {
                return Some(hex[..7].to_string());
            }
        }
    }
    None
}

fn main() {
    let root = repo_root();

    // VUICK_GIT_SHA：`git -C <root> rev-parse --short HEAD`；失败 → 空串（消费方显示
    // unknown）。
    let git_sha = {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&root).args(["rev-parse", "--short", "HEAD"]);
        first_line_ok(&mut cmd).unwrap_or_default()
    };
    println!("cargo:rustc-env=VUICK_GIT_SHA={git_sha}");

    // VUICK_RUSTC：用 Cargo 传入的 `RUSTC` 环境变量指向的编译器跑 `--version`；
    // 失败 → "unknown"。
    let rustc = {
        let mut cmd = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()));
        cmd.arg("--version");
        first_line_ok(&mut cmd).unwrap_or_else(|| "unknown".into())
    };
    println!("cargo:rustc-env=VUICK_RUSTC={rustc}");

    // VUICK_GPUI_VERSION：仓库根 vendor/zed/crates/gpui/Cargo.toml 的 version 字段。
    let gpui_version = {
        let path = root.join("vendor/zed/crates/gpui/Cargo.toml");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| read_version_field(&t))
            .unwrap_or_default()
    };
    println!("cargo:rustc-env=VUICK_GPUI_VERSION={gpui_version}");

    // VUICK_ZED_REV：仓库根 Cargo.lock 里 `zed?rev=…` 的短 rev（前 7 位）。
    let zed_rev = {
        let path = root.join("Cargo.lock");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| extract_zed_rev(&t))
            .unwrap_or_default()
    };
    println!("cargo:rustc-env=VUICK_ZED_REV={zed_rev}");

    // VUICK_TARGET：Cargo 只为 build script 提供 `TARGET` 运行时环境变量（普通 crate
    // 编译期没有），故在此透传成 `env!("VUICK_TARGET")`（构建档·目标行消费）。
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=VUICK_TARGET={target}");
}
