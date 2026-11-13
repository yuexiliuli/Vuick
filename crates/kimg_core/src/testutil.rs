//! 仅测试/探针共享的工具（语料目录装配等）。
//!
//! 各格式 codec crate 与本 crate 的探针此前各自复制一份「`CARGO_MANIFEST_DIR` 向上两级 +
//! `Benchmarks/corpus/<fmt>`」的拼装逻辑。此处收敛为单点；测试行为与 SKIP 约定保持不变
//! （语料缺失 = SKIP 常态，见 AGENTS.md K56）。

use std::path::{Path, PathBuf};

/// 语料目录装配：`env_key` 命中优先（调用方原有 env 键名逐处传回，键名与语义不变）；
/// 否则默认 `<workspace>/Benchmarks/corpus/<fmt>`（workspace = `CARGO_MANIFEST_DIR` 向上两级，
/// 源码树 target，不受 .cargo target-dir 重定向影响，K66）。
///
/// 目录存在才返回 `Some`；缺失返回 `None`，由调用方按各自旧语义 SKIP 或 panic（SKIP 常态，
/// 不得视为回归）。
pub fn corpus_dir(env_key: &str, fmt: &str) -> Option<PathBuf> {
    let p = match std::env::var(env_key) {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
            ws.join("Benchmarks").join("corpus").join(fmt)
        }
    };
    p.is_dir().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径守卫：默认语料路径 = `<workspace>/Benchmarks/corpus/<fmt>`（本机有语料 → Some）。
    #[test]
    fn default_corpus_dir_points_at_workspace() {
        let dir = corpus_dir("KIMG_TESTUTIL_UNSET_ENV", "PNG");
        match dir {
            Some(p) => assert!(
                p.ends_with("Benchmarks\\corpus\\PNG") || p.ends_with("Benchmarks/corpus/PNG"),
                "默认语料路径异常: {p:?}"
            ),
            None => eprintln!("testutil 守卫：本机缺 Benchmarks/corpus/PNG → None（SKIP 口径）"),
        }
    }

    /// 缺失目录 → None（SKIP 分流，调用方不 panic）。
    #[test]
    fn missing_dir_returns_none() {
        let dir = corpus_dir("KIMG_TESTUTIL_UNSET_ENV", "NO_SUCH_FMT");
        assert!(dir.is_none());
    }

    /// env 覆盖优先：以子进程派生本探针（规避 ed2024 `env::set_var` unsafe，
    /// 与 pngsimd 跨进程姿势一致）。正常套件直接跑本测试（env 未设）→ 打印跳过不失败。
    #[test]
    fn env_override_wins_via_subprocess() {
        let exe = std::env::current_exe().expect("当前测试可执行路径");
        // libtest 全名不带 crate 前缀（module_path!() 带），剥前缀对齐（pngsimd 同款姿势）。
        let crate_pref = format!("{}::", env!("CARGO_PKG_NAME"));
        let exact = format!("{}::probe_env_override", module_path!())
            .strip_prefix(&crate_pref)
            .unwrap_or(&module_path!())
            .to_string();
        let out = std::process::Command::new(&exe)
            .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
            .env("KIMG_TESTUTIL_PROBE_DIR", "C:\\")
            .output()
            .expect("子进程（env 覆盖探针）运行失败");
        assert!(
            out.status.success(),
            "子进程失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("TESTUTIL_ENV_OVERRIDE OK"),
            "子进程未走 env 覆盖分支：{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// 由 [`env_override_wins_via_subprocess`] 以 env 派生；直接运行（env 未设）
    /// 落到默认语料路径，打印跳过。
    #[test]
    fn probe_env_override() {
        let p = corpus_dir("KIMG_TESTUTIL_PROBE_DIR", "PNG");
        if p.as_deref() == Some(Path::new("C:\\")) {
            println!("TESTUTIL_ENV_OVERRIDE OK");
        } else {
            eprintln!("probe 直接运行时 env 未设，语料 = {p:?}，跳过");
        }
    }
}
