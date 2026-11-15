//! 环境变量门控诊断探针（`GPUI_EDITOR_IMG_DIAG=1` 启用），零生产语义。
//!
//! 采集（全部在主线程回调内；门控关闭 = 一次静态读 + 分支短路，无 `Instant` 计时、无分配）：
//! - **帧间隔分布**：`render()` 入口两帧间隔 ms 入桶（<8 / 8-16 / 16-33 / 33-66 / >66ms），
//!   识别离群帧（>33ms 即时打一行 `LONG_FRAME`，附「落盘后 ±100ms」相关性标记 `*`）；
//! - **switch→目标就绪→首帧上屏时延**：`switch()` 时刻 t0 → `install_current()` 时刻 t1
//!   （`decode_ms`，含缓存命中瞬时 / 解码池等待）→ 下一次 `render()` 画到当前图时刻 t2
//!   （`paint_lag` = 落盘→上屏，量化「滚到头才出图」与「落盘→上传/重绘」）。
//!
//! 仅统计「正在滚动」窗口：最后一次 `switch()` 之后 [`SCROLL_ACTIVE_MS`] 内的 render 才入帧
//! 间隔桶，避免空闲 / 悬停帧把直方图稀释；滚动期外帧间隔不延续（首格不吞空闲间隔）。

use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

/// 门控起效的 env 变量值。
const ENV_VAR: &str = "GPUI_EDITOR_IMG_DIAG";
/// 摘要打印间隔（累计多少帧打一次直方图，避免刷屏）。
const REPORT_EVERY: u64 = 25;
/// 离群帧阈值（ms）：帧间隔超过即认为主线程 render/上传/布局有阻塞。
const LONG_FRAME_MS: f64 = 33.0;
/// 「离群帧紧跟落盘」判定窗口（ms）：落盘后该窗口内出现离群帧 → 打相关性标记。
const INSTALL_CORR_WIN_MS: f64 = 100.0;
/// 「滚动期」窗口（ms）：最后一次 `switch()` 后该窗口内的 render 计入帧间隔统计。
const SCROLL_ACTIVE_MS: u64 = 2000;

/// 门控是否开启：收敛自 `base::diag::env_flag`（进程级 per-env 缓存；本批 Pattern 7）。
/// 原实现 `env::var(ENV_VAR).is_ok_and(|v| v == "1")` 与 `matches!(as_deref(), Ok("1"))`
/// 逐语义等价（默认只认 `"1"`，其余值/缺失 → 关）。
fn enabled() -> bool {
    base::diag::env_flag(ENV_VAR)
}

/// 时延统计（decode_ms / paint_lag 共用）。
#[derive(Default)]
struct Stat {
    n: u64,
    sum_ms: f64,
    max_ms: f64,
}

impl Stat {
    fn add(&mut self, ms: f64) {
        self.n += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
    }
    fn avg(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_ms / self.n as f64
        }
    }
}

#[derive(Default)]
struct DiagState {
    /// 帧间隔直方图（5 桶：<8 / 8-16 / 16-33 / 33-66 / >66ms）。
    buckets: [u64; 5],
    frames: u64,
    frame_max: f64,
    frame_sum: f64,
    last_frame: Option<Instant>,
    /// 最近一次切图的时刻（滚动期判定 + 相关性）。
    last_switch: Option<Instant>,
    /// 累计切图次数（含量：每帧平均/最大消化多少切图 = 是否追尾欠账）。
    total_switches: u64,
    /// 上一次统计帧时的切图总数（计算「每帧耗掉的切图数」）。
    switches_at_frame: u64,
    /// 切图（滚轮输入）到达间隔统计：与帧间隔对比判「应用追不追得上输入」。
    input: Stat,
    /// 在途「切图→落盘」记录：(目标路径, t0)。
    pending_switch: Option<(std::path::PathBuf, Instant)>,
    /// 已落盘、待本图首帧上屏：(目标路径, t1)。
    pending_install: Option<(std::path::PathBuf, Instant)>,
    decode: Stat,
    paint: Stat,
    since_report: u64,
    /// 单帧消化切图数上限（>1 说明输入堆积、应用在追尾）。
    max_switches_per_frame: u64,
}

thread_local! {
    static DIAG: RefCell<Option<DiagState>> = RefCell::new(None);
}

/// 门控启用时取可变诊断态执行 `f`；未启用 / 无态时直接短路（零开销）。
fn with<F: FnOnce(&mut DiagState)>(f: F) {
    if !enabled() {
        return;
    }
    DIAG.with(|d| {
        let mut d = d.borrow_mut();
        if d.is_none() {
            *d = Some(DiagState::default());
        }
        if let Some(st) = d.as_mut() {
            f(st);
        }
    });
}

fn bucket_ms(ms: f64) -> usize {
    for (i, b) in [8.0, 16.0, 33.0, 66.0].iter().enumerate() {
        if ms < *b {
            return i;
        }
    }
    4
}

/// `switch()` 命中目标：记录 t0 与目标路径（滚动期判定 + decode 分账起点）。
pub(crate) fn note_switch(path: &Path) {
    with(|st| {
        let now = Instant::now();
        st.total_switches += 1;
        if let Some(t) = st.last_switch {
            st.input.add(now.duration_since(t).as_secs_f64() * 1e3);
        }
        st.last_switch = Some(now);
        st.pending_switch = Some((path.to_path_buf(), now));
    });
}

/// `install_current()` 落盘：结算 decode_ms = t1 - t0，并置待上屏记录。
pub(crate) fn note_install(path: &Path) {
    with(|st| {
        let t1 = Instant::now();
        // 核对路径一致（多查看器交错时丢弃错配在途记录；探针简化，够诊断用）
        let decode_ms = match &st.pending_switch {
            Some((p, t0)) if *p == *path => Some(t1.duration_since(*t0).as_secs_f64() * 1e3),
            _ => None,
        };
        st.pending_switch = None;
        if let Some(ms) = decode_ms {
            st.decode.add(ms);
            eprintln!("[IMG-DIAG] decode_ms={ms:.1}");
        }
        st.pending_install = Some((path.to_path_buf(), t1));
    });
}

/// `render()` 入口：结算 paint_lag（首个含当前图的 render）+ 滚动期帧间隔入桶。
pub(crate) fn note_render(path: &Path, has_image: bool) {
    with(|st| {
        let now = Instant::now();

        // 首帧上屏时延：pending_install 命中当前路径且已挂图 → t2。
        if has_image
            && let Some((p, t1)) = &st.pending_install
            && *p == *path
        {
            let ms = now.duration_since(*t1).as_secs_f64() * 1e3;
            st.paint.add(ms);
            st.pending_install = None;
            eprintln!("[IMG-DIAG] paint_lag={ms:.1}");
        }

        // 帧间隔：仅统计「滚动期」（最近切图后窗口内）；滚动期外不延续上一帧，
        // 避免首格吞掉空闲间隔被误判为主线程阻塞。
        let scrolling = st
            .last_switch
            .is_some_and(|t| now.duration_since(t).as_millis() as u64 <= SCROLL_ACTIVE_MS);
        if scrolling {
            // 本帧消化了几个切图（>1 = 输入堆积追尾欠账）
            let consumed = st.total_switches - st.switches_at_frame;
            st.switches_at_frame = st.total_switches;
            st.max_switches_per_frame = st.max_switches_per_frame.max(consumed);
            if let Some(prev) = st.last_frame.take() {
                let ms = now.duration_since(prev).as_secs_f64() * 1e3;
                st.frames += 1;
                st.frame_sum += ms;
                st.frame_max = st.frame_max.max(ms);
                st.buckets[bucket_ms(ms)] += 1;
                st.since_report += 1;
                if ms > LONG_FRAME_MS {
                    // 相关性：离群帧是否紧跟落盘（落盘后 ±INSTALL_CORR_WIN_MS 内）
                    let since_install = st
                        .pending_install
                        .as_ref()
                        .map(|(_, t1)| now.duration_since(*t1).as_secs_f64() * 1e3)
                        .unwrap_or(f64::INFINITY);
                    let corr = if since_install <= INSTALL_CORR_WIN_MS {
                        " INSTALL_CORR*"
                    } else {
                        ""
                    };
                    eprintln!("[IMG-DIAG] LONG_FRAME={ms:.1}ms{corr}");
                }
                if st.since_report >= REPORT_EVERY {
                    st.report();
                }
            }
            st.last_frame = Some(now);
        } else {
            st.last_frame = None;
        }
    });
}

impl DiagState {
    /// 打印直方图 + 各平均时延摘要（前缀 `[IMG-DIAG]`，供 stderr 重定向抓取）。
    fn report(&mut self) {
        self.since_report = 0;
        let n = self.frames.max(1);
        eprintln!(
            "[IMG-DIAG] frames={} interval(max={:.1} avg={:.1}ms) buckets(<8:{} 8-16:{} 16-33:{} 33-66:{} >66:{}) input(avg={:.1} max={:.1} n={}) swPerFrameMax={} decode(avg={:.1} max={:.1} n={}) paint(avg={:.1} max={:.1} n={})",
            self.frames,
            self.frame_max,
            self.frame_sum / n as f64,
            self.buckets[0], self.buckets[1], self.buckets[2], self.buckets[3], self.buckets[4],
            self.input.avg(), self.input.max_ms, self.input.n,
            self.max_switches_per_frame,
            self.decode.avg(), self.decode.max_ms, self.decode.n,
            self.paint.avg(), self.paint.max_ms, self.paint.n,
        );
    }
}
