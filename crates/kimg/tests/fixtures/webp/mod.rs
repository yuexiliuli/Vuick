//! WebP 测试语料定位助手（外部语料，不入仓）。
//!
//! 本仓不再保存 `*.webp` 图片副本；测试按名在以下候选目录里查找（按序，取存在的）：
//! 1. `KIMG_WEBP_FIXTURES` 环境变量（`;` 分隔多目录，最高优先）
//! 2. `target/imgbench-corpus/WEBP`（gitignored 快照）
//! 3. `E:\Rust\SimdCodecs\测试图片\WEBP`（用户主语料，001-036.webp）
//! 4. `E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp`（SimdCodecs 上游
//!    夹具目录，golden TSV 对应文件名所在）
//!
//! 单元测试（`src/codecs/webp/*.rs` 内 `#[cfg(test)]`）经 `#[path]` 引入本模块：
//! `#[path = "../../../tests/fixtures/webp/mod.rs"] mod webp_fixtures_support;`；
//! 集成测试（`tests/*.rs`）经 `mod fixtures;` → `fixtures::webp::...`。
//! 缺失时打印醒目 `SKIP: missing webp fixtures (set KIMG_WEBP_FIXTURES=...)`
//! （AGENTS §5 convention），不因单文件缺失而崩整测试。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

static MISSING_WARNED: Once = Once::new();

static GOLDEN_STATIC: OnceLock<BTreeSet<String>> = OnceLock::new();

/// 静态 golden manifest 文件名集合（52，SimdCodecs 金标准；读仓内 TSV 一次）。
/// 逐字节 parity 硬闸以该集合为口径：金标准图不匹配=失败；越界语料（用户/imgbench-corpus）
/// 仅记录不判失败（见 AGENTS §5「找到几个测几个」）。
fn golden_static_names() -> &'static BTreeSet<String> {
    GOLDEN_STATIC.get_or_init(|| {
        let mut set = BTreeSet::new();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/webp/webp-oracle-golden.tsv");
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.trim_end();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() == 6 {
                    set.insert(parts[0].to_lowercase());
                }
            }
        }
        set
    })
}

/// `name` 是否属于静态 golden manifest（金标准硬闸对象；SimdCodecs 上游 52 图）。
pub fn is_golden_static_name(name: &str) -> bool {
    golden_static_names().contains(&name.to_lowercase())
}

/// 候选语料目录原始列表（含不存在者；顺序即优先级）。
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // 1. 环境变量（`;` 分隔多目录）。
    if let Ok(val) = std::env::var("KIMG_WEBP_FIXTURES") {
        for part in val.split(';') {
            let p = PathBuf::from(part.trim());
            if !p.as_os_str().is_empty() {
                dirs.push(p);
            }
        }
    }
    // 2. target/imgbench-corpus/WEBP（gitignored 快照）。
    dirs.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("WEBP"),
    );
    // 3. 用户主语料。
    dirs.push(PathBuf::from(r"E:\Rust\SimdCodecs\测试图片\WEBP"));
    // 4. SimdCodecs 上游夹具目录。
    dirs.push(PathBuf::from(
        r"E:\Rust\SimdCodecs\tests\Sdcb.SimdCodecs.Tests\Fixtures\webp",
    ));
    dirs
}

/// 返回实际存在的候选目录（按序去重）。全部缺失时打印一次醒目 SKIP。
pub fn webp_fixture_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for d in candidate_dirs() {
        if d.is_dir() && !out.contains(&d) {
            out.push(d);
        }
    }
    if out.is_empty() {
        MISSING_WARNED.call_once(|| {
            eprintln!(
                "SKIP: missing webp fixtures (set KIMG_WEBP_FIXTURES=... 或放置语料到 \
                 E:\\Rust\\SimdCodecs\\测试图片\\WEBP)"
            );
        });
    }
    out
}

/// 按文件名在全部候选目录里定位（含子目录一层内同名，如子目录里也有 Astronaut.webp）；
/// 找不到返回 None。
pub fn webp_fixture_path(name: &str) -> Option<PathBuf> {
    for dir in webp_fixture_dirs() {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let nested = p.join(name);
                    if nested.is_file() {
                        return Some(nested);
                    }
                }
            }
        }
    }
    None
}

/// 全部候选目录里的 `.webp` 文件名（去重排序；含子目录一层）。供「整目录驱动」用例用，
/// 不含非图片文件（version.txt / golden TSV / 脚本等）。
pub fn webp_fixture_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for dir in webp_fixture_dirs() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file() && p.extension().map_or(false, |x| x == "webp") {
                    if let Some(n) = p.file_name() {
                        names.push(n.to_string_lossy().into_owned());
                    }
                } else if p.is_dir() {
                    if let Ok(sub) = std::fs::read_dir(&p) {
                        for se in sub.flatten() {
                            let sp = se.path();
                            if sp.is_file() && sp.extension().map_or(false, |x| x == "webp") {
                                if let Some(n) = sp.file_name() {
                                    names.push(n.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}
