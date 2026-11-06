#![forbid(unsafe_code)]
use super::emit::EmitScratch;
use super::simd::raw;

// ---------------------------------------------------------------------------
// 每线程解码固定缓冲池（结构线唯一实靶：S0 production 复核定案 setup 1.55% + glue 3.19% ≈
// 每调用一次固定分配：planes 随分配/首触缺页 + above_b/above_nz/row_filters/EmitScratch 分配）。
// 把「每调用一次」的固定分配改为「每线程复用池」：第二次起的 decode 从池 take（容量不够才
// grow，永不 shrink），decode 结束 Drop 归还 → 不再分配、不再首触缺页。
//
// 线程安全（K28）：thread_local = 每线程独立、无跨线程共享 → 无数据竞争。串行路径单线程自然
// 安全；MT 路径的 planes（planes_A）虽被 t2 重建线程写入，其**生命周期**由调用线程的 guard
// 全程持有（`std::thread::scope` join 后才 Drop 归还），不跨线程归还。t2/t3 各自的
// planes_B / EmitScratch（`filter_t3_loop` 内）是逐解码 spawn 的线程局部 per-call 分配，
// 超出本线范围（见 brief），留原样。
//
// 生命周期（RAII）：decode 开始 take、decode 结束 Drop return；使用中的缓冲绝不归还池。
// 同线程嵌套 decode（递归/库内互调）时内层 `take` 拿到的池为空 → 以新建兜底（等价「每调用
// 新建」语义），外层归还后内层仍由内层 guard 持有 → 重入安全。
//
// 峰值驻留：每线程最多 hold 历史最大一帧的各缓冲（grow 不 shrink）。量级 ≈
//   planes：ch*y_pitch + 2*uh*uv_pitch（padded 全平面，~1 帧 YUV420）
//   above_b：mbw*4 B（i4 上邻 b 模式，微小）；above_nz：mbw*4 B；row_filters：mbw*mbh*2 B
//   EmitScratch：4*width + 8*(cw+8) B（上采样工作区）
// ---------- ----------------------------------------------------------------- -

/// 池内各固定缓冲（`take` 时按需 grow、永不 shrink；线程存活期驻留 = 历史最大一帧）。
#[derive(Default)]
pub(crate) struct Vp8SubBuffers {
    pub(crate) planes: Vec<u8>,
    pub(crate) above_b: Vec<u8>,
    pub(crate) above_nz: Vec<u32>,
    pub(crate) row_filters: Vec<(u8, u8)>,
    pub(crate) scratch: Option<EmitScratch>,
}

impl Vp8SubBuffers {
    /// const 空池（thread_local 初始化用；`Default` 非 const，故手写）。
    const fn empty() -> Self {
        Vp8SubBuffers {
            planes: Vec::new(),
            above_b: Vec::new(),
            above_nz: Vec::new(),
            row_filters: Vec::new(),
            scratch: None,
        }
    }
}

thread_local! {
    static POOL: std::cell::RefCell<Vp8SubBuffers> =
        const { std::cell::RefCell::new(Vp8SubBuffers::empty()) };
}

/// RAII guard：构造时从本线程池 whole-take 缓冲并按本帧尺寸就绪，Drop 时归还。
pub(crate) struct Vp8SubBuffersGuard {
    bufs: Option<Vp8SubBuffers>,
}

impl Vp8SubBuffersGuard {
    /// 从本线程池取缓冲并按当前帧尺寸就绪（grow，永不 shrink）。各缓冲就绪语义：
    /// - `planes`：长度 = y_vec_len + 2*uv_vec_len，**不 memset**（未初始化语义——调用方
    ///   129 左列确定性初始化 + 预测/IDCT 逐宏块整块写满先于读，见 `decode_from_header`）。
    /// - `above_b`/`above_nz`：首宏块行在写前的读取依赖 0 初值（`vec![0; _]` 原语义）→ 每帧
    ///   取用时确定性重初始化（fill 清零）当前长度，不能复用上一帧残值。
    /// - `row_filters`：串行路径逐元素写满先于读（每宏块写、经受滤波行才读）→ 无需清零。
    ///   MT 路径不消费本表（行包自带滤波参数），亦无读。
    pub(crate) fn take(y_vec_len: usize, uv_vec_len: usize, mbw: usize, mbh: usize) -> Self {
        let mut b = POOL.with(|p| std::mem::take(&mut *p.borrow_mut()));
        // planes：只保证 `len==need`（兼收缩到当前帧），容量保留；grow 用 reserve（不触碰新页，
        // 首触缺页仍由重建写触发）——与 `uninit_vec_u8` 未初始化语义完全一致。
        let need = y_vec_len + 2 * uv_vec_len;
        if b.planes.capacity() < need {
            b.planes.reserve(need - b.planes.len());
        }
        // SAFETY：u8 无 Drop glue，set_len 后未初始化尾部保证读前写满（上方纪律 + 调用点
        // Safety 注释）；收缩仅截断已写字节、容量保留（永不 shrink）。set_len 下沉
        // `raw::vec_set_len`（未初始化不变量随门面传递）。
        raw::vec_set_len(&mut b.planes, need);
        // above_b：length 就位 + 全量清零（vec![0; mbw*4] 原语义每帧重立）。
        if b.above_b.capacity() < mbw * 4 {
            b.above_b.reserve(mbw * 4 - b.above_b.len());
        }
        // SAFETY：同上（u8 无 Drop；下方 fill 全覆盖后再移交读方）。
        raw::vec_set_len(&mut b.above_b, mbw * 4);
        b.above_b.fill(0);
        // above_nz：同 above_b（vec![0u32; mbw] 原语义）。
        if b.above_nz.capacity() < mbw {
            b.above_nz.reserve(mbw - b.above_nz.len());
        }
        // SAFETY：u32 无 Drop glue；fill 全覆盖。
        raw::vec_set_len(&mut b.above_nz, mbw);
        b.above_nz.fill(0);
        // row_filters：长度就位即够（全写先于读证明，见 `decode_from_header`）。(u8,u8) 无
        // Drop glue，set_len 安全。
        if b.row_filters.capacity() < mbw * mbh {
            b.row_filters.reserve(mbw * mbh - b.row_filters.len());
        }
        raw::vec_set_len(&mut b.row_filters, mbw * mbh);
        Vp8SubBuffersGuard { bufs: Some(b) }
    }

    pub(crate) fn take_planes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bufs.as_mut().expect("guard alive").planes)
    }

    pub(crate) fn take_above_b(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bufs.as_mut().expect("guard alive").above_b)
    }

    pub(crate) fn take_above_nz(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.bufs.as_mut().expect("guard alive").above_nz)
    }

    pub(crate) fn take_row_filters(&mut self) -> Vec<(u8, u8)> {
        std::mem::take(&mut self.bufs.as_mut().expect("guard alive").row_filters)
    }

    /// 上采样工作区：池中复用则 resize 到当前 `width`（各字段每次 emit 前全写先于读，见
    /// `EmitScratch::resize_to`），池空则新建。
    pub(crate) fn take_scratch(&mut self, width: usize) -> EmitScratch {
        let old = self.bufs.as_mut().expect("guard alive").scratch.take();
        match old {
            Some(s) => s.resize_to(width),
            None => EmitScratch::new(width),
        }
    }

    pub(crate) fn return_planes(&mut self, v: Vec<u8>) {
        self.bufs.as_mut().expect("guard alive").planes = v;
    }

    pub(crate) fn return_above_b(&mut self, v: Vec<u8>) {
        self.bufs.as_mut().expect("guard alive").above_b = v;
    }

    pub(crate) fn return_above_nz(&mut self, v: Vec<u32>) {
        self.bufs.as_mut().expect("guard alive").above_nz = v;
    }

    pub(crate) fn return_row_filters(&mut self, v: Vec<(u8, u8)>) {
        self.bufs.as_mut().expect("guard alive").row_filters = v;
    }

    pub(crate) fn return_scratch(&mut self, s: EmitScratch) {
        self.bufs.as_mut().expect("guard alive").scratch = Some(s);
    }
}

impl Drop for Vp8SubBuffersGuard {
    fn drop(&mut self) {
        if let Some(b) = self.bufs.take() {
            POOL.with(|p| *p.borrow_mut() = b);
        }
    }
}
