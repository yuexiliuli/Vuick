//! 分阶段粗钟探针（HEIF 解码；K96 参照优先分账第一阶段，只测量不优化）。
//!
//! - 普通构建（无 `stage_profile` feature）：所有包装都是 `f()` 直通（`#[inline(always)]`，
//!   零开销、不改字节、oracle 全绿不受影响）。
//! - `feature="stage_profile"`（默认关闭）：各阶段累计墙钟到进程级原子，测试 `perf_stage_profile`
//!   （`tests/stage_profile.rs`，release）真墙钟跑多轮中位，输出各阶段 ms 与占全链 %。
//!
//! 阶段对齐 C# `quick --ablate` 内插口径（`docs/bench/ref_000_heif_ablate.log`）：
//! `parse`=容器+切片+CABAC+残差语法（等效 ablate 的 parse-only）、`intra`=帧内预测、
//! `residual`=逆变换+残差加、`deblock`、`sao`、`color`=色转、`grid`=grid 合成、
//! `transform`=transform 应用。只允许在函数/语句边界包瞬时计时，禁止改任何解码逻辑。

#[cfg(feature = "stage_profile")]
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
#[cfg(feature = "stage_profile")]
use std::time::Instant;

#[cfg(feature = "stage_profile")]
static P_PARSE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_INTRA: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_RESIDUAL: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_DEBLOCK: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_SAO: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_GRID: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_COLOR: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "stage_profile")]
static P_TRANSFORM: AtomicU64 = AtomicU64::new(0);

/// 阶段序（与 `snapshot`/测试表对齐）：parse, intra, residual, deblock, sao, grid, color, transform。
pub const STAGE_NAMES: [&str; 8] = [
    "parse", "intra", "residual", "deblock", "sao", "grid", "color", "transform",
];

macro_rules! stage_wrap {
    ($name:ident, $static:ident) => {
        /// 计时到阶段累计；feature 外 `f()` 直通（零开销）。
        #[inline(always)]
        pub fn $name<R>(f: impl FnOnce() -> R) -> R {
            #[cfg(feature = "stage_profile")]
            {
                let t = Instant::now();
                let r = f();
                $static.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                return r;
            }
            #[cfg(not(feature = "stage_profile"))]
            {
                f()
            }
        }
    };
}

stage_wrap!(parse, P_PARSE);
stage_wrap!(intra, P_INTRA);
stage_wrap!(residual, P_RESIDUAL);
stage_wrap!(deblock, P_DEBLOCK);
stage_wrap!(sao, P_SAO);
stage_wrap!(grid, P_GRID);
stage_wrap!(color, P_COLOR);
stage_wrap!(transform, P_TRANSFORM);

/// 清零一次解码的分账累计（测试每轮开头调用）。
#[cfg(feature = "stage_profile")]
pub fn reset() {
    for s in [
        &P_PARSE, &P_INTRA, &P_RESIDUAL, &P_DEBLOCK, &P_SAO, &P_GRID, &P_COLOR, &P_TRANSFORM,
    ] {
        s.store(0, Relaxed);
    }
}
#[cfg(not(feature = "stage_profile"))]
pub fn reset() {}

/// 各阶段累计毫秒（与 `STAGE_NAMES` 对齐的 8 元数组）。
#[cfg(feature = "stage_profile")]
pub fn snapshot() -> [f64; 8] {
    [
        P_PARSE.load(Relaxed) as f64 / 1e6,
        P_INTRA.load(Relaxed) as f64 / 1e6,
        P_RESIDUAL.load(Relaxed) as f64 / 1e6,
        P_DEBLOCK.load(Relaxed) as f64 / 1e6,
        P_SAO.load(Relaxed) as f64 / 1e6,
        P_GRID.load(Relaxed) as f64 / 1e6,
        P_COLOR.load(Relaxed) as f64 / 1e6,
        P_TRANSFORM.load(Relaxed) as f64 / 1e6,
    ]
}
#[cfg(not(feature = "stage_profile"))]
pub fn snapshot() -> [f64; 8] {
    [0.0; 8]
}
